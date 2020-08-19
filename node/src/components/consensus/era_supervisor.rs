//! Consensus service is a component that will be communicating with the reactor.
//! It will receive events (like incoming message event or create new message event)
//! and propagate them to the underlying consensus protocol.
//! It tries to know as little as possible about the underlying consensus. The only thing
//! it assumes is the concept of era/epoch and that each era runs separate consensus instance.
//! Most importantly, it doesn't care about what messages it's forwarding.

use std::{
    collections::HashMap,
    fmt::{self, Debug, Formatter},
    rc::Rc,
};

use anyhow::Error;
use casperlabs_types::U512;
use num_traits::AsPrimitive;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::{
    components::{
        chainspec_loader::HighwayConfig,
        consensus::{
            consensus_protocol::{BlockContext, ConsensusProtocol, ConsensusProtocolResult},
            highway_core::validators::Validators,
            protocols::highway::{HighwayContext, HighwayProtocol, HighwaySecret},
            traits::NodeIdT,
            Config, ConsensusMessage, Event, ReactorEventT,
        },
    },
    crypto::asymmetric_key,
    crypto::{
        asymmetric_key::{PublicKey, SecretKey},
        hash,
    },
    effect::{EffectBuilder, EffectExt, Effects},
    types::{Block, FinalizedBlock, Instruction, Motes, ProtoBlock, Timestamp},
    utils::WithDir,
};

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EraId(pub(crate) u64);

impl EraId {
    fn message(self, payload: Vec<u8>) -> ConsensusMessage {
        ConsensusMessage {
            era_id: self,
            payload,
        }
    }

    fn successor(self) -> EraId {
        EraId(self.0 + 1)
    }
}

pub(crate) struct EraSupervisor<I> {
    /// A map of active consensus protocols.
    /// A value is a trait so that we can run different consensus protocol instances per era.
    active_eras: HashMap<EraId, Box<dyn ConsensusProtocol<I, ProtoBlock, PublicKey>>>,
    pub(super) secret_signing_key: Rc<SecretKey>,
    pub(super) public_signing_key: PublicKey,
    validator_stakes: Vec<(PublicKey, Motes)>,
    current_era: EraId,
    highway_config: HighwayConfig,
}

impl<I> Debug for EraSupervisor<I> {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        let ae: Vec<_> = self.active_eras.keys().collect();
        write!(formatter, "EraSupervisor {{ active_eras: {:?}, .. }}", ae)
    }
}

impl<I> EraSupervisor<I>
where
    I: NodeIdT,
{
    pub(crate) fn new<REv: ReactorEventT<I>>(
        timestamp: Timestamp,
        config: WithDir<Config>,
        effect_builder: EffectBuilder<REv>,
        validator_stakes: Vec<(PublicKey, Motes)>,
        highway_config: &HighwayConfig,
    ) -> Result<(Self, Effects<Event<I>>), Error> {
        let (root, config) = config.into_parts();
        let secret_signing_key = Rc::new(config.secret_key_path.load(root)?);
        let public_signing_key = PublicKey::from(secret_signing_key.as_ref());

        let mut era_supervisor = Self {
            active_eras: Default::default(),
            secret_signing_key,
            public_signing_key,
            current_era: EraId(0),
            validator_stakes: validator_stakes.clone(),
            highway_config: *highway_config,
        };

        let effects = era_supervisor.new_era(effect_builder, EraId(0), timestamp, validator_stakes);

        Ok((era_supervisor, effects))
    }

    fn new_era<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        era_id: EraId,
        timestamp: Timestamp,
        validator_stakes: Vec<(PublicKey, Motes)>,
    ) -> Effects<Event<I>> {
        if self.active_eras.contains_key(&era_id) {
            panic!("{:?} already exists", era_id);
        }
        let sum_stakes: Motes = validator_stakes.iter().map(|(_, stake)| *stake).sum();
        let validators: Validators<PublicKey> = if sum_stakes.value() > U512::from(u64::MAX) {
            validator_stakes
                .into_iter()
                .map(|(key, stake)| {
                    let weight = stake.value() / (sum_stakes.value() / (u64::MAX / 2));
                    (key, AsPrimitive::<u64>::as_(weight))
                })
                .collect()
        } else {
            validator_stakes
                .into_iter()
                .map(|(key, stake)| (key, AsPrimitive::<u64>::as_(stake.value())))
                .collect()
        };

        let instance_id = hash::hash(format!("Highway era {}", era_id.0));
        let secret =
            HighwaySecret::new(Rc::clone(&self.secret_signing_key), self.public_signing_key);
        let ftt = validators.total_weight()
            * u64::from(self.highway_config.finality_threshold_percent)
            / 100;
        let (highway, effects) = HighwayProtocol::<I, HighwayContext>::new(
            instance_id,
            validators,
            0, // TODO: get a proper seed ?
            self.public_signing_key,
            secret,
            self.highway_config.minimum_round_exponent,
            ftt,
            timestamp,
        );

        let _ = self.active_eras.insert(era_id, Box::new(highway));

        effects
            .into_iter()
            .flat_map(|result| self.handle_consensus_result(era_id, effect_builder, result))
            .collect()
    }

    fn handle_consensus_result<REv: ReactorEventT<I>>(
        &mut self,
        era_id: EraId,
        effect_builder: EffectBuilder<REv>,
        consensus_result: ConsensusProtocolResult<I, ProtoBlock, PublicKey>,
    ) -> Effects<Event<I>> {
        match consensus_result {
            ConsensusProtocolResult::InvalidIncomingMessage(msg, sender, error) => {
                // TODO: we will probably want to disconnect from the sender here
                // TODO: Print a more readable representation of the message.
                error!(
                    ?msg,
                    ?sender,
                    ?error,
                    "invalid incoming message to consensus instance"
                );
                Default::default()
            }
            ConsensusProtocolResult::CreatedGossipMessage(out_msg) => {
                // TODO: we'll want to gossip instead of broadcast here
                effect_builder
                    .broadcast_message(era_id.message(out_msg))
                    .ignore()
            }
            ConsensusProtocolResult::CreatedTargetedMessage(out_msg, to) => effect_builder
                .send_message(to, era_id.message(out_msg))
                .ignore(),
            ConsensusProtocolResult::ScheduleTimer(timestamp) => {
                let timediff = timestamp.saturating_sub(Timestamp::now());
                effect_builder
                    .set_timeout(timediff.into())
                    .event(move |_| Event::Timer { era_id, timestamp })
            }
            ConsensusProtocolResult::CreateNewBlock {
                block_context,
                opt_parent,
            } => effect_builder
                .request_proto_block(block_context, opt_parent)
                .event(move |(proto_block, block_context)| Event::NewProtoBlock {
                    era_id,
                    proto_block,
                    block_context,
                }),
            ConsensusProtocolResult::FinalizedBlock {
                value: proto_block,
                new_equivocators,
                rewards,
                timestamp,
            } => {
                // Announce the finalized proto block.
                let mut effects = effect_builder
                    .announce_finalized_proto_block(proto_block.clone())
                    .ignore();
                if proto_block.switch_block() {
                    assert_eq!(
                        era_id, self.current_era,
                        "finalized block in unexpected era"
                    );
                    self.current_era_mut().deactivate_validator();
                    let new_era_id = era_id.successor();
                    let validator_stakes = self.validator_stakes.clone();
                    effects.extend(self.new_era(
                        effect_builder,
                        new_era_id,
                        timestamp,
                        validator_stakes,
                    ));
                    self.current_era = new_era_id;
                }
                // Create instructions for slashing equivocators.
                let mut instructions: Vec<_> = new_equivocators
                    .into_iter()
                    .map(Instruction::Slash)
                    .collect();
                if !rewards.is_empty() {
                    instructions.push(Instruction::Rewards(rewards));
                };
                // Request execution of the finalized block.
                let fb = FinalizedBlock::new(proto_block, timestamp, instructions);
                effects.extend(
                    effect_builder
                        .execute_block(fb)
                        .event(move |block| Event::ExecutedBlock { era_id, block }),
                );
                effects
            }
            ConsensusProtocolResult::ValidateConsensusValue(sender, proto_block) => effect_builder
                .validate_proto_block(sender.clone(), proto_block)
                .event(move |(is_valid, proto_block)| {
                    if is_valid {
                        Event::AcceptProtoBlock {
                            era_id,
                            proto_block,
                        }
                    } else {
                        Event::InvalidProtoBlock {
                            era_id,
                            sender,
                            proto_block,
                        }
                    }
                }),
        }
    }

    pub(crate) fn delegate_to_era<F, REv>(
        &mut self,
        era_id: EraId,
        effect_builder: EffectBuilder<REv>,
        f: F,
    ) -> Effects<Event<I>>
    where
        REv: ReactorEventT<I>,
        F: FnOnce(
            &mut dyn ConsensusProtocol<I, ProtoBlock, PublicKey>,
        ) -> Result<Vec<ConsensusProtocolResult<I, ProtoBlock, PublicKey>>, Error>,
    {
        match self.active_eras.get_mut(&era_id) {
            None => {
                if era_id > self.current_era {
                    info!("received message for future {:?}", era_id);
                } else {
                    info!("received message for obsolete {:?}", era_id);
                }
                Effects::new()
            }
            Some(consensus) => match f(&mut **consensus) {
                Ok(results) => results
                    .into_iter()
                    .flat_map(|result| self.handle_consensus_result(era_id, effect_builder, result))
                    .collect(),
                Err(error) => {
                    error!(%error, ?era_id, "got error from era id {:?}: {:?}", era_id, error);
                    Effects::new()
                }
            },
        }
    }

    pub(super) fn handle_timer<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        era_id: EraId,
        timestamp: Timestamp,
    ) -> Effects<Event<I>> {
        self.delegate_to_era(era_id, effect_builder, move |consensus| {
            consensus.handle_timer(timestamp)
        })
    }

    pub(super) fn handle_message<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        sender: I,
        msg: ConsensusMessage,
    ) -> Effects<Event<I>> {
        let ConsensusMessage { era_id, payload } = msg;
        self.delegate_to_era(era_id, effect_builder, move |consensus| {
            consensus.handle_message(sender, payload)
        })
    }

    pub(super) fn handle_new_proto_block<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        era_id: EraId,
        proto_block: ProtoBlock,
        block_context: BlockContext,
    ) -> Effects<Event<I>> {
        let mut effects = effect_builder
            .announce_proposed_proto_block(proto_block.clone())
            .ignore();
        effects.extend(
            self.delegate_to_era(era_id, effect_builder, move |consensus| {
                consensus.propose(proto_block, block_context)
            }),
        );
        effects
    }

    pub(super) fn handle_executed_block<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _era_id: EraId,
        mut block: Block,
    ) -> Effects<Event<I>> {
        // TODO - we should only sign if we're a validator for the given era ID.
        let signature = asymmetric_key::sign(
            block.hash().inner(),
            &self.secret_signing_key,
            &self.public_signing_key,
        );
        block.append_proof(signature);
        effect_builder
            .put_block_to_storage(Box::new(block))
            .ignore()
    }

    pub(super) fn handle_accept_proto_block<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        era_id: EraId,
        proto_block: ProtoBlock,
    ) -> Effects<Event<I>> {
        let mut effects = self.delegate_to_era(era_id, effect_builder, |consensus| {
            consensus.resolve_validity(&proto_block, true)
        });
        effects.extend(
            effect_builder
                .announce_proposed_proto_block(proto_block)
                .ignore(),
        );
        effects
    }

    pub(super) fn handle_invalid_proto_block<REv: ReactorEventT<I>>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        era_id: EraId,
        _sender: I,
        proto_block: ProtoBlock,
    ) -> Effects<Event<I>> {
        self.delegate_to_era(era_id, effect_builder, |consensus| {
            consensus.resolve_validity(&proto_block, false)
        })
    }

    /// Returns the current era.
    fn current_era_mut(&mut self) -> &mut Box<dyn ConsensusProtocol<I, ProtoBlock, PublicKey>> {
        self.active_eras
            .get_mut(&self.current_era)
            .expect("current era does not exist")
    }

    /// Inspect the active eras.
    #[cfg(test)]
    pub fn active_eras(
        &self,
    ) -> &HashMap<EraId, Box<dyn ConsensusProtocol<I, ProtoBlock, PublicKey>>> {
        &self.active_eras
    }
}
