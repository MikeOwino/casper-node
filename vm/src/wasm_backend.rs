pub(crate) mod wasmer;
use bytes::Bytes;
use thiserror::Error;

use crate::{
    storage::{Address, GlobalStateReader, TrackingCopy},
    Config, Executor, VMError, VMResult,
};

#[derive(Debug)]
pub struct GasUsage {
    /// The amount of gas used by the execution.
    pub(crate) gas_limit: u64,
    /// The amount of gas remaining after the execution.
    pub(crate) remaining_points: u64,
}

impl GasUsage {
    pub fn gas_spent(&self) -> u64 {
        debug_assert!(self.remaining_points <= self.gas_limit);
        self.gas_limit - self.remaining_points
    }
}

/// Container that holds all relevant modules necessary to process an execution request.
pub struct Context<S: GlobalStateReader, E: Executor> {
    pub address: Address,
    pub storage: TrackingCopy<S>,
    pub executor: E,
}

#[derive(Debug, Copy, Clone)]
pub enum MeteringPoints {
    Remaining(u64),
    Exhausted,
}

impl MeteringPoints {
    pub fn try_into_remaining(self) -> Result<u64, Self> {
        if let Self::Remaining(v) = self {
            Ok(v)
        } else {
            Err(self)
        }
    }
}

/// An abstraction over the 'caller' object of a host function that works for any Wasm VM.
///
/// This allows access for important instances such as the context object that was passed to the
/// instance, wasm linear memory access, etc.

pub(crate) trait Caller<S: GlobalStateReader, E: Executor> {
    fn config(&self) -> &Config;
    fn context(&self) -> &Context<S, E>;
    fn context_mut(&mut self) -> &mut Context<S, E>;
    /// Returns currently running *unmodified* bytecode.
    fn bytecode(&self) -> Bytes;

    fn memory_read(&self, offset: u32, size: usize) -> VMResult<Vec<u8>> {
        let mut vec = vec![0; size];
        self.memory_read_into(offset, &mut vec)?;
        Ok(vec)
    }
    fn memory_read_into(&self, offset: u32, output: &mut [u8]) -> VMResult<()>;
    fn memory_write(&self, offset: u32, data: &[u8]) -> VMResult<()>;
    /// Allocates memory inside the Wasm VM by calling an export.
    ///
    /// Error is a type-erased error coming from the VM itself.
    fn alloc(&mut self, idx: u32, size: usize, ctx: u32) -> VMResult<u32>;
    /// Returns the amount of gas used.
    fn gas_consumed(&mut self) -> MeteringPoints;
    /// Set the amount of gas used.
    fn consume_gas(&mut self, value: u64) -> MeteringPoints;
}

#[derive(Debug, Error)]
pub enum PreparationError {
    #[error("Missing export {0}")]
    MissingExport(String),
    #[error("Compile error: {0}")]
    Compile(String),
    #[error("Memory instantiation error: {0}")]
    Memory(String),
    #[error("Instantiation error: {0}")]
    Instantiation(String),
}

pub(crate) trait WasmInstance<S: GlobalStateReader, E: Executor> {
    fn call_export(&mut self, name: &str) -> (Result<(), VMError>, GasUsage);
    fn call_function(&mut self, function_index: u32) -> (Result<(), VMError>, GasUsage);
    fn teardown(self) -> Context<S, E>;
}
