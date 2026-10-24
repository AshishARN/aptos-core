// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

pub(crate) mod vm_wrapper;

// // Add these to your existing imports at the top of the file:
// use once_cell::sync::Lazy;
// use move_core_types::language_storage::ModuleId;
// use move_binary_format::CompiledModule;
// use move_vm_runtime::Module;
// use aptos_types::vm::modules::AptosModuleExtension;

use crate::counters::{BLOCK_EXECUTOR_CONCURRENCY, BLOCK_EXECUTOR_EXECUTE_BLOCK_SECONDS};
use aptos_aggregator::{
    delayed_change::DelayedChange, delta_change_set::DeltaOp, resolver::TAggregatorV1View,
};
use rayon::prelude::*;
use aptos_types::transaction::TransactionPayload;
use aptos_types::transaction::TransactionExecutableRef;
use aptos_block_executor::{
    code_cache_global_manager::AptosModuleCacheManager,
    errors::BlockExecutionError,
    executor::BlockExecutor,
    task::{
        AfterMaterializationOutput, BeforeMaterializationOutput, ExecutorTask,
        TransactionOutput as BlockExecutorTransactionOutput,
    },
    txn_commit_hook::TransactionCommitHook,
    txn_provider::TxnProvider,
    types::InputOutputKey,
};
use aptos_logger::info;
use aptos_types::{
    block_executor::{
        config::BlockExecutorConfig, transaction_slice_metadata::TransactionSliceMetadata,
    },
    contract_event::ContractEvent,
    error::{code_invariant_error, PanicError},
    fee_statement::FeeStatement,
    state_store::{state_key::StateKey, state_value::StateValueMetadata, StateView, StateViewId},
    transaction::{
        signature_verified_transaction::SignatureVerifiedTransaction, AuxiliaryInfo, BlockOutput,
        TransactionOutput, TransactionStatus,
    },
    write_set::WriteOp,
};
use aptos_vm_logging::{flush_speculative_logs, init_speculative_logs};
use aptos_vm_types::{
    abstract_write_op::AbstractResourceWriteOp, module_write_set::ModuleWrite, output::VMOutput,
    resolver::ResourceGroupSize,
};
use move_core_types::{
    language_storage::StructTag,
    value::MoveTypeLayout,
    vm_status::{StatusCode, VMStatus},
};
use move_vm_runtime::execution_tracing::Trace;
use move_vm_types::delayed_values::delayed_field_id::DelayedFieldID;
use once_cell::sync::OnceCell;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use triomphe::Arc as TriompheArc;
use vm_wrapper::AptosExecutorTask;

/// Thread pool used by `execute_block` for parallel transaction execution.
///
/// Stores `(pool, num_threads)` so we can detect when the requested concurrency level
/// changes and recreate the pool. In production the concurrency level is set once at
/// startup, so the pool is created once and the mutex is uncontended thereafter.
/// Benchmarking and debugging tools may iterate over multiple concurrency levels in the
/// same process, which requires replacing the pool.

// ADD THESE IMPORTS AT THE TOP
use once_cell::sync::Lazy;
use move_core_types::language_storage::ModuleId;
use move_binary_format::CompiledModule;
use move_vm_runtime::Module;
use aptos_types::vm::modules::AptosModuleExtension;
use std::ops::Deref;
use move_binary_format::access::ModuleAccess;
use dashmap::DashMap;

// ---------------------------------------------------------------------------
// DAG_LOG: zero-cost debug logging gate
//
// Wrap any diagnostic println!/eprintln! in dag_log!(...) instead.
// In release builds (benchmark runs) this expands to nothing — not even the
// argument expressions are evaluated, so there is zero runtime cost.
// In debug builds, or when the DAG_DEBUG env var is set at compile time,
// it prints to stderr with a [DAG] prefix.
//
// Usage:
//   dag_log!("block has {} txns", num_txns);
//   dag_log!("wave {}: {:?}", idx, wave);
//
// To re-enable for a debugging session without touching the code, build with:
//   DAG_DEBUG=1 cargo build   (add a cfg flag in build.rs, or use --features dag-debug)
// Or simply swap `#[cfg(debug_assertions)]` to `#[cfg(any())]` to silence even debug.
// ---------------------------------------------------------------------------
#[cfg(debug_assertions)]
macro_rules! dag_log {
    ($($arg:tt)*) => { eprintln!("[DAG] {}", format_args!($($arg)*)) };
}
#[cfg(not(debug_assertions))]
macro_rules! dag_log {
    ($($arg:tt)*) => {};
}

// A global JIT cache for the analyzer to remember function summaries across blocks
static SYMBOLIC_RW_CACHE: Lazy<DashMap<crate::analyzer::FunctionID, crate::analyzer::abstract_domain::FunctionSummary>> = 
    Lazy::new(|| DashMap::new());

// NEW: Tracks which functions are currently being analyzed to prevent 32 threads from doing duplicate work
static IN_FLIGHT_ANALYSIS: Lazy<DashMap<crate::analyzer::FunctionID, Arc<Mutex<()>>>> = 
    Lazy::new(|| DashMap::new());

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct ConcretePath {
    resource: String,
    address: String,
    is_unknown: bool,
}

// O(1) RAM Cache Fetcher for the Analyzer!
fn fetch_module_and_dependencies<S: aptos_types::state_store::StateView + Sync>(
    global_cache: &aptos_block_executor::code_cache_global::GlobalModuleCache<ModuleId, CompiledModule, Module, AptosModuleExtension>,
    state_view: &S,
    module_id: &ModuleId,
    module_map: &mut HashMap<ModuleId, CompiledModule>
) {
    if module_map.contains_key(module_id) { return; }

    let compiled_module_opt = if let Some(module_code) = global_cache.get(module_id) {
        Some(module_code.code().deserialized().as_ref().clone())
    } else {
        let state_key = aptos_types::state_store::state_key::StateKey::module(
            module_id.address(),
            module_id.name(),
        );
        if let Ok(Some(state_value)) = state_view.get_state_value(&state_key) {
            move_binary_format::CompiledModule::deserialize(state_value.bytes()).ok()
        } else {
            None
        }
    };

    if let Some(compiled_module) = compiled_module_opt {
        module_map.insert(module_id.clone(), compiled_module.clone());
        for dep_handle in compiled_module.module_handles() {
            let dep_id = compiled_module.module_id_for_handle(dep_handle);
            if dep_id != *module_id {
                fetch_module_and_dependencies(global_cache, state_view, &dep_id, module_map);
            }
        }
    }
}

fn is_publish_transaction(txn: &SignatureVerifiedTransaction) -> bool {
    if let SignatureVerifiedTransaction::Valid(aptos_types::transaction::Transaction::UserTransaction(u)) = txn {
        if let TransactionPayload::ModuleBundle(_) = u.payload() { return true; }
        if let Ok(TransactionExecutableRef::EntryFunction(entry_fn)) = u.payload().executable_ref() {
            if entry_fn.module().address() == &move_core_types::account_address::AccountAddress::ONE 
                && entry_fn.module().name().as_str() == "code" 
                && entry_fn.function().as_str() == "publish_package_txn" {
                return true;
            }
        }
    }
    false
}

static RAYON_EXEC_POOL: Mutex<Option<(Arc<rayon::ThreadPool>, usize)>> = Mutex::new(None);

/// Output type wrapper used by block executor. VM output is stored first, then
/// transformed into TransactionOutput type that is returned.
#[derive(Debug)]
pub struct AptosTransactionOutput {
    vm_output: Option<VMOutput>,
    committed_output: OnceCell<TransactionOutput>,
}

impl AptosTransactionOutput {
    pub fn new(output: VMOutput) -> Self {
        Self {
            vm_output: Some(output),
            committed_output: OnceCell::new(),
        }
    }

    fn take_output(mut self) -> TransactionOutput {
        match self.committed_output.take() {
            Some(output) => output,
            // TODO: revisit whether we should always get it via committed, or o.w. create a
            // dedicated API without creating empty data structures.
            // This is currently used because we do not commit skip_output() transactions.
            None => self
                .vm_output
                .take()
                .expect("Output must be set")
                .into_transaction_output()
                .expect("Transaction output is not already materialized"),
        }
    }
}

pub struct AfterMaterializationGuard<'a> {
    output: &'a TransactionOutput,
}

impl<'a> AfterMaterializationOutput<SignatureVerifiedTransaction>
    for AfterMaterializationGuard<'a>
{
    fn fee_statement(&self) -> FeeStatement {
        if let Ok(Some(fee_statement)) = self.output.try_extract_fee_statement() {
            return fee_statement;
        }
        FeeStatement::zero()
    }

    fn has_new_epoch_event(&self) -> bool {
        self.output.has_new_epoch_event()
    }
}

/// Before materialization guard wrapper that holds a read lock.
pub struct BeforeMaterializationGuard<'a> {
    guard: &'a VMOutput,
}

impl BeforeMaterializationOutput<SignatureVerifiedTransaction> for BeforeMaterializationGuard<'_> {
    fn fee_statement(&self) -> FeeStatement {
        *self.guard.fee_statement()
    }

    fn has_new_epoch_event(&self) -> bool {
        self.guard
            .events()
            .iter()
            .map(|(event, _)| event)
            .any(ContractEvent::is_new_epoch_event)
    }

    fn output_approx_size(&self) -> u64 {
        self.guard.materialized_size()
    }

    fn get_write_summary(&self) -> HashSet<InputOutputKey<StateKey, StructTag>> {
        let mut writes = HashSet::new();

        for (state_key, write) in self.guard.resource_write_set() {
            match write {
                AbstractResourceWriteOp::Write(_)
                | AbstractResourceWriteOp::WriteWithDelayedFields(_) => {
                    writes.insert(InputOutputKey::Resource(state_key.clone()));
                },
                AbstractResourceWriteOp::WriteResourceGroup(write) => {
                    for tag in write.inner_ops().keys() {
                        writes.insert(InputOutputKey::Group(state_key.clone(), tag.clone()));
                    }
                },
                AbstractResourceWriteOp::InPlaceDelayedFieldChange(_)
                | AbstractResourceWriteOp::ResourceGroupInPlaceDelayedFieldChange(_) => {
                    // No conflicts on resources from in-place delayed field changes.
                    // Delayed fields conflicts themselves are handled via
                    // delayed_field_change_set below.
                },
            }
        }

        for identifier in self.guard.delayed_field_change_set().keys() {
            writes.insert(InputOutputKey::DelayedField(*identifier));
        }

        writes
    }

    // TODO: get rid of the cloning data-structures in the following APIs.
    fn resource_group_write_set(
        &self,
    ) -> HashMap<
        StateKey,
        (
            WriteOp,
            ResourceGroupSize,
            BTreeMap<StructTag, (WriteOp, Option<TriompheArc<MoveTypeLayout>>)>,
        ),
    > {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::WriteResourceGroup(group_write) = write {
                    Some((
                        key.clone(),
                        (
                            group_write.metadata_op().clone(),
                            group_write
                                .maybe_group_op_size()
                                .unwrap_or(ResourceGroupSize::zero_combined()),
                            group_write
                                .inner_ops()
                                .iter()
                                .map(|(tag, (op, maybe_layout))| {
                                    (tag.clone(), (op.clone(), maybe_layout.clone()))
                                })
                                .collect(),
                        ),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    fn for_each_resource_key_no_aggregator_v1(
        &self,
        callback: &mut dyn FnMut(&StateKey) -> Result<(), PanicError>,
    ) -> Result<(), PanicError> {
        for key in self
            .guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| match write {
                AbstractResourceWriteOp::Write(_)
                | AbstractResourceWriteOp::WriteWithDelayedFields(_) => Some(key),
                _ => None,
            })
        {
            callback(key)?;
        }
        Ok(())
    }

    fn for_each_resource_group_key_and_tags(
        &self,
        callback: &mut dyn FnMut(&StateKey, HashSet<&StructTag>) -> Result<(), PanicError>,
    ) -> Result<(), PanicError> {
        for (key, tags) in self
            .guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::WriteResourceGroup(group_write) = write {
                    Some((key, group_write.inner_ops().keys().collect()))
                } else {
                    None
                }
            })
        {
            callback(key, tags)?;
        }

        Ok(())
    }

    /// More efficient implementation to avoid unnecessarily cloning inner_ops.
    fn resource_group_metadata_ops(&self) -> Vec<(StateKey, WriteOp)> {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::WriteResourceGroup(group_write) = write {
                    Some((key.clone(), group_write.metadata_op().clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn resource_write_set(
        &self,
    ) -> HashMap<StateKey, (TriompheArc<WriteOp>, Option<TriompheArc<MoveTypeLayout>>)> {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| match write {
                AbstractResourceWriteOp::Write(write_op) => {
                    Some((key.clone(), (TriompheArc::new(write_op.clone()), None)))
                },
                AbstractResourceWriteOp::WriteWithDelayedFields(write) => Some((
                    key.clone(),
                    (
                        TriompheArc::new(write.write_op.clone()),
                        Some(write.layout.clone()),
                    ),
                )),
                _ => None,
            })
            .collect()
    }

    /// Should never be called after incorporating materialized output, as that consumes vm_output.
    fn module_write_set(&self) -> &BTreeMap<StateKey, ModuleWrite<WriteOp>> {
        self.guard.module_write_set()
    }

    /// Should never be called after incorporating materialized output, as that consumes vm_output.
    fn aggregator_v1_write_set(&self) -> BTreeMap<StateKey, WriteOp> {
        self.guard.aggregator_v1_write_set().clone()
    }

    /// Should never be called after incorporating materialized output, as that consumes vm_output.
    fn aggregator_v1_delta_set(&self) -> BTreeMap<StateKey, DeltaOp> {
        self.guard.aggregator_v1_delta_set().clone()
    }

    /// Should never be called after incorporating materialized output, as that consumes vm_output.
    fn delayed_field_change_set(&self) -> BTreeMap<DelayedFieldID, DelayedChange<DelayedFieldID>> {
        self.guard.delayed_field_change_set().clone()
    }

    fn reads_needing_delayed_field_exchange(
        &self,
    ) -> Vec<(StateKey, StateValueMetadata, TriompheArc<MoveTypeLayout>)> {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::InPlaceDelayedFieldChange(change) = write {
                    Some((key.clone(), change.metadata.clone(), change.layout.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn group_reads_needing_delayed_field_exchange(&self) -> Vec<(StateKey, StateValueMetadata)> {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::ResourceGroupInPlaceDelayedFieldChange(change) =
                    write
                {
                    Some((key.clone(), change.metadata.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Should never be called after incorporating materialized output, as that consumes vm_output.
    fn get_events(&self) -> Vec<(ContractEvent, Option<MoveTypeLayout>)> {
        self.guard.events().to_vec()
    }

    // For legacy interfaces, there are more efficient alternatives in BlockSTMv2.
    // For now we do get the benefits of comparing different implementations.
    // TODO: consider adjusting sequential execution and BlockSTMv1 to use the superior
    // patterns and remove these legacy interfaces (needs to be done carefully).
    //
    // Internally clones and also allocates a new vector. Used for BlockSTMv1 only.
    fn legacy_v1_resource_group_tags(&self) -> Vec<(StateKey, HashSet<StructTag>)> {
        self.guard
            .resource_write_set()
            .iter()
            .flat_map(|(key, write)| {
                if let AbstractResourceWriteOp::WriteResourceGroup(group_write) = write {
                    Some((
                        key.clone(),
                        group_write.inner_ops().keys().cloned().collect(),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }
}

impl BlockExecutorTransactionOutput for AptosTransactionOutput {
    type AfterMaterializationGuard<'a> = AfterMaterializationGuard<'a>;
    type BeforeMaterializationGuard<'a> = BeforeMaterializationGuard<'a>;
    type Txn = SignatureVerifiedTransaction;

    fn committed_output(&self) -> &OnceCell<TransactionOutput> {
        &self.committed_output
    }

    /// Execution output for transactions that comes after SkipRest signal or when there was a
    /// problem creating the output (e.g. group serialization issue).
    fn skip_output() -> Self {
        Self::new(VMOutput::empty_with_status(TransactionStatus::Retry))
    }

    fn discard_output(discard_code: StatusCode) -> Self {
        Self::new(VMOutput::empty_with_status(TransactionStatus::Discard(
            discard_code,
        )))
    }

    fn before_materialization<'a>(&'a self) -> Result<BeforeMaterializationGuard<'a>, PanicError> {
        Ok(BeforeMaterializationGuard {
            guard: self
                .vm_output
                .as_ref()
                .ok_or_else(|| code_invariant_error("Output must be set but not materialized"))?,
        })
    }

    fn after_materialization<'a>(&'a self) -> Result<AfterMaterializationGuard<'a>, PanicError> {
        Ok(AfterMaterializationGuard {
            output: self
                .committed_output
                .get()
                .ok_or_else(|| code_invariant_error("Output must be materialized"))?,
        })
    }

    fn is_materialized_and_success(&self) -> bool {
        if let Some(output) = self.committed_output.get() {
            return output
                .status()
                .as_kept_status()
                .is_ok_and(|status| status.is_success());
        }
        false
    }

    fn check_materialization(&self) -> Result<bool, PanicError> {
        if let Some(output) = self.committed_output.get() {
            if output.status().is_retry() {
                return Err(code_invariant_error(
                    "Committed output must not have is_retry set.",
                ));
            }
            Ok(true)
        } else {
            if !self
                .vm_output
                .as_ref()
                .is_some_and(|output| output.status().is_retry())
            {
                return Err(code_invariant_error(
                    "Non-committed output must exist with is_retry set.",
                ));
            }
            Ok(false)
        }
    }

    fn incorporate_materialized_txn_output(
        &mut self,
        aggregator_v1_writes: Vec<(StateKey, WriteOp)>,
        materialized_resource_write_set: Vec<(StateKey, WriteOp)>,
        materialized_events: Vec<ContractEvent>,
    ) -> Result<Trace, PanicError> {
        // Before creating the output, extract the trace for replay.
        let mut vm_output = self
            .vm_output
            .take()
            .expect("Output must be set to incorporate materialized data");
        let trace = vm_output.take_trace();

        self.committed_output
            .set(
                vm_output.into_transaction_output_with_materialized_write_set(
                    aggregator_v1_writes,
                    materialized_resource_write_set,
                    materialized_events,
                )?,
            )
            .map_err(|_| {
                code_invariant_error(
                    "Could not combine VMOutput with the materialized resource and event data",
                )
            })?;
        Ok(trace)
    }

    fn set_txn_output_for_non_dynamic_change_set(&mut self) {
        assert!(
            self.committed_output
                .set(
                    self.vm_output
                        .take()
                        .expect("Output must be set to incorporate materialized data")
                        .into_transaction_output()
                        .expect("We should be able to always convert to transaction output"),
                )
                .is_ok(),
            "Could not combine VMOutput with the materialized resource and event data"
        );
    }

    // Used only by the sequential execution, does not set committed_output.
    fn legacy_sequential_materialize_agg_v1(
        &mut self,
        view: &impl TAggregatorV1View<Identifier = StateKey>,
    ) {
        self.vm_output
            .as_mut()
            .expect("Output must be set to incorporate materialized data")
            .try_materialize(view)
            .expect("Delta materialization failed");
    }
}

pub struct AptosBlockExecutorWrapper<
    E: ExecutorTask<
        Txn = SignatureVerifiedTransaction,
        Error = VMStatus,
        Output = AptosTransactionOutput,
    >,
> {
    _phantom: PhantomData<E>,
}

impl<
        E: ExecutorTask<
            Txn = SignatureVerifiedTransaction,
            AuxiliaryInfo = AuxiliaryInfo,
            Error = VMStatus,
            Output = AptosTransactionOutput,
        >,
    > AptosBlockExecutorWrapper<E>
{
    pub fn execute_block_on_thread_pool<
    S: StateView + Sync,
    L: TransactionCommitHook,
    TP: TxnProvider<SignatureVerifiedTransaction, AuxiliaryInfo> + Sync,
>(
    executor_thread_pool: Arc<rayon::ThreadPool>,
    signature_verified_block: &TP,
    state_view: &S,
    module_cache_manager: &AptosModuleCacheManager,
    config: BlockExecutorConfig,
    transaction_slice_metadata: TransactionSliceMetadata,
    transaction_commit_listener: Option<L>,
) -> Result<BlockOutput<SignatureVerifiedTransaction, TransactionOutput>, VMStatus> {
    let _timer = BLOCK_EXECUTOR_EXECUTE_BLOCK_SECONDS.start_timer();

    let num_txns = signature_verified_block.num_txns();
    if state_view.id() != StateViewId::Miscellaneous {
        init_speculative_logs(num_txns);
    }

    BLOCK_EXECUTOR_CONCURRENCY.set(config.local.concurrency_level as i64);

    let mut module_cache_manager_guard = module_cache_manager.try_lock(
        &state_view,
        &config.local.module_cache_config,
        transaction_slice_metadata.clone(),
    )?;

    // =========================================================
    // 🚀 THE DAG SCHEDULE BUILDER 🚀
    // =========================================================
    let mut waves: Option<(Vec<Vec<usize>>, Vec<Vec<usize>>, Vec<usize>)> = None;

    if num_txns > 1 {
        dag_log!("Intercepted block with {} txns! Starting static analysis...", num_txns);
        let dag_start = std::time::Instant::now();
        let analysis_start = std::time::Instant::now();
        let cache_hits = std::sync::atomic::AtomicUsize::new(0);
        let cache_misses = std::sync::atomic::AtomicUsize::new(0);
        let global_cache = module_cache_manager_guard.module_cache();

        // STEP 1A: STATIC ANALYSIS — parallel over txns, warm cache hit common case
        //
        // intern_map and intern_counter are hoisted outside the par_iter block so that
        // STEP 1A.5 can intern additional strings (BlockMetadata write set + prologue
        // read set) into the same integer id space, enabling correct data-dependency
        // edges instead of a blunt unconditional barrier.
        let intern_map: dashmap::DashMap<String, u32> = dashmap::DashMap::new();
        let intern_counter = std::sync::atomic::AtomicU32::new(0);
        // Per-block concrete result cache.
        // Key: func_id string + raw arg bytes concatenated into a single Vec<u8>.
        // Value: (read_keys, write_keys) already concretized and interned.
        type ConcreteKey = (String, Vec<Vec<u8>>);
        let concrete_cache: dashmap::DashMap<String, (Vec<(u32,u32,bool)>, Vec<(u32,u32,bool)>)> =
            dashmap::DashMap::new();

        let mut concrete_sets: Vec<(Vec<(u32, u32, bool)>, Vec<(u32, u32, bool)>, bool)> = {
            use rayon::prelude::*;

            // intern_str borrows the hoisted maps. DashMap + AtomicU32 are both
            // Sync, so this closure is Sync and safe to use across rayon threads.
            let intern_str = |s: String| -> u32 {
                // Fast path: already interned
                if let Some(id) = intern_map.get(&s) {
                    return *id;
                }
                // Slow path: race to insert. fetch_add gives a unique candidate id.
                // entry().or_insert() ensures only one id wins for this string.
                // We must re-read after or_insert to get the actual winner's id.
                let candidate = intern_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *intern_map.entry(s).or_insert(candidate)
                //                   ^^^^^^^^^^^^^^^^^^^ returns &V of whatever is in the map.
                // If we won the race, this is candidate. If we lost, this is the prior winner's id.
                // Either way it's the canonical id for this string. intern_counter may have gaps
                // (unused candidate ids from losers) but that's fine — ids just need to be stable.
            };

            let state_view_ref = state_view;  // already &S, just name it explicitly

            (0..num_txns).into_par_iter().map(|i| {
                let txn = signature_verified_block.get_txn(i as u32);
                if is_publish_transaction(&txn) {
                    return (Vec::new(), Vec::new(), false);
                }

                let mut read_keys: Vec<(u32, u32, bool)> = Vec::new();
                let mut write_keys: Vec<(u32, u32, bool)> = Vec::new();
                let mut is_analyzable = false;

                if let SignatureVerifiedTransaction::Valid(
                    aptos_types::transaction::Transaction::UserTransaction(u)
                ) = txn {
                    if let Ok(TransactionExecutableRef::EntryFunction(entry_fn)) =
                        u.payload().executable_ref()
                    {

                        // eprintln!("[SENDER] txn={} sender={}", i, u.sender().to_hex_literal());
                        is_analyzable = true;
                        let func_id = crate::analyzer::FunctionID {
                            address: entry_fn.module().address().to_hex_literal(),
                            module:  entry_fn.module().name().to_string(),
                            name:    entry_fn.function().to_string(),
                        };

                        if SYMBOLIC_RW_CACHE.get(&func_id).is_none() {
                            // Grab a specific lock for this function ID
                            let lock = IN_FLIGHT_ANALYSIS
                                .entry(func_id.clone())
                                .or_insert_with(|| Arc::new(Mutex::new(())))
                                .clone();
                                
                            // Only one thread gets inside this guard at a time for this specific function
                            let _guard = lock.lock().unwrap();

                            // Double-check the cache! Another thread might have finished the analysis while we were waiting for the lock
                            if SYMBOLIC_RW_CACHE.get(&func_id).is_none() {
                                cache_misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let mut loaded_modules = HashMap::new();
                                fetch_module_and_dependencies(
                                    global_cache, state_view_ref, entry_fn.module(), &mut loaded_modules,
                                );
                                // add println statement to show the transaction number:
                                eprintln!("[BLOCK-EXECUTOR] Analyzing txn {}", i);
                                crate::analyzer::analyze_function_with_dependencies(
                                    &func_id, &loaded_modules, &SYMBOLIC_RW_CACHE,
                                );
                            } else {
                                cache_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        } else {
                            cache_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }

                        if let Some(symbolic_summary_ref) = SYMBOLIC_RW_CACHE.get(&func_id) {
                            let symbolic_summary = symbolic_summary_ref.value();

                            // ── FAST PATH: concrete cache ──────────────────────────────
                            let cache_key = {
                                let mut k = func_id.to_string();
                                k.push('|');
                                k.push_str(&u.sender().to_hex_literal());
                                for arg in entry_fn.args() {
                                    k.push('|');
                                    k.push_str(&hex::encode(arg));
                                }
                                k
                            };
                            if let Some(cached) = concrete_cache.get(&cache_key) {
                                let (rk, wk) = cached.value().clone();
                                read_keys = rk;
                                write_keys = wk;
                                return (read_keys, write_keys, is_analyzable);
                            }
                            // ── SLOW PATH: concretize + intern ─────────────────────────

                            // Change concrete_args to hold an array of addresses per argument!
                            let mut concrete_args: Vec<Vec<String>> = vec![vec![u.sender().to_hex_literal()]];
                            for arg in entry_fn.args() {
                                if arg.len() == 32 {
                                    if let Ok(a) = move_core_types::account_address::AccountAddress::from_bytes(arg.as_slice()) {
                                        concrete_args.push(vec![a.to_hex_literal()]);
                                        continue;
                                    }
                                } 
                                // DECODE BCS vector<address>: 1 length byte + N * 32 bytes
                                else if arg.len() > 1 && (arg.len() - 1) % 32 == 0 {
                                    let num_addresses = (arg.len() - 1) / 32;
                                    if arg[0] as usize == num_addresses {
                                        let mut vec_addrs = Vec::new();
                                        for i in 0..num_addresses {
                                            let start = 1 + i * 32;
                                            if let Ok(a) = move_core_types::account_address::AccountAddress::from_bytes(&arg[start..start+32]) {
                                                vec_addrs.push(a.to_hex_literal());
                                            }
                                        }
                                        concrete_args.push(vec_addrs);
                                        continue;
                                    }
                                }
                                concrete_args.push(vec![hex::encode(arg)]);
                            }
                            let concrete_ty_args: Vec<String> = entry_fn.ty_args().iter().map(|t| format!("{:?}", t)).collect();

                            let concretize = |prov: &crate::analyzer::abstract_domain::Provenance| -> Vec<String> {
                                use crate::analyzer::abstract_domain::Provenance;
                                fn fmt_prov(prov: &Provenance, c_args: &Vec<Vec<String>>, c_t_args: &Vec<String>) -> Vec<String> {
                                    match prov {
                                        Provenance::Param(idx) => c_args.get(*idx).cloned().unwrap_or_else(|| vec!["UNKNOWN".into()]),
                                        Provenance::Global { resource_type, address_source } => {
                                            let addrs = fmt_prov(address_source, c_args, c_t_args);
                                            addrs.into_iter().map(|a| format!("{}@{}", resource_type, a)).collect()
                                        },
                                        Provenance::Const(val) => vec![val.clone()],
                                        Provenance::TypeArg(idx) => {
                                            if let Some(ty_str) = c_t_args.get(*idx) {
                                                if let Some(addr_start) = ty_str.find("address: ") {
                                                    let addr_part = &ty_str[addr_start + 9..];
                                                    if let Some(addr_end) = addr_part.find(',') {
                                                        let hex = &addr_part[..addr_end].trim_start_matches('0');
                                                        let res = if hex.is_empty() { "0x1".to_string() } else { format!("0x{}", hex) };
                                                        return vec![res];
                                                    }
                                                }
                                            }
                                            vec!["UNKNOWN".into()]
                                        },
                                        _ => vec!["UNKNOWN".into()],
                                    }
                                }
                                let roots = prov.flatten();
                                if roots.is_empty() { return vec!["UNKNOWN".into()]; }
                                roots.iter().flat_map(|r| fmt_prov(r, &concrete_args, &concrete_ty_args)).collect()
                            };

                            let sender_addr = u.sender().to_hex_literal();

                            // Implicit writes: sequence number + gas coin
                            let account_res_id = intern_str(
                                "0x1::account::Account".to_string(),
                            );
                            let gas_res_id = intern_str(
                                "0x1::coin::CoinStore<0x1::aptos_coin::AptosCoin>"
                                    .to_string(),
                            );
                            let fa_store_res_id = intern_str(
                                "0x1::fungible_asset::FungibleStore".to_string(),
                            );
                            let sender_id = intern_str(sender_addr.clone());
                            write_keys.push((account_res_id, sender_id, false));
                            write_keys.push((gas_res_id, sender_id, false));
                            write_keys.push((fa_store_res_id, sender_id, false));

                            let entry_module = entry_fn.module().name().as_str();
                            let entry_func   = entry_fn.function().as_str();

                            // // MANUAL DAG INJECTION: Guarantee receiver conflicts for all transfer variants
                            // if entry_module == "aptos_account" || entry_module == "coin" || entry_module == "fungible_asset" {
                            //     if entry_func.contains("transfer") || entry_func.contains("deposit") {
                            //         // Argument 1 is the receiver (or array of receivers for batch_transfer)
                            //         if let Some(receivers) = concrete_args.get(1) {
                            //             for receiver in receivers {
                            //                 if receiver != "UNKNOWN" && receiver.starts_with("0x") {
                            //                     let recv_id = intern_str(receiver.clone());
                            //                     write_keys.push((fa_store_res_id, recv_id, false));
                            //                     write_keys.push((gas_res_id, recv_id, false));
                            //                     write_keys.push((account_res_id, recv_id, false));
                            //                 }
                            //             }
                            //         }
                            //     }
                            // }

                            let writes_singleton_resources = matches!(
                                (entry_module, entry_func),
                                ("fungible_asset",           "create_store")
                                | ("fungible_asset",         "remove_store")
                                | ("fungible_asset",         "mint")
                                | ("fungible_asset",         "mint_to")
                                | ("fungible_asset",         "burn")
                                | ("fungible_asset",         "burn_from")
                                | ("fungible_asset",         "set_frozen_flag")
                                | ("fungible_asset",         "upgrade_to_concurrent")
                                | ("managed_fungible_asset", "mint")
                                | ("managed_fungible_asset", "mint_to")
                                | ("managed_fungible_asset", "burn")
                                | ("managed_fungible_asset", "burn_from")
                                | ("managed_fungible_asset", "set_frozen_flag")
                                | ("coin",                   "initialize")
                                | ("coin",                   "initialize_internal")
                                | ("coin",                   "mint")
                                | ("coin",                   "burn")
                                | ("coin",                   "burn_from")
                                | ("coin",                   "upgrade_supply")
                                | ("coin",                   "destroy_zero")
                                | ("aptos_coin",             "mint")
                                | ("aptos_coin",             "initialize")
                                | ("aptos_account",          "create_account")
                                | ("aptos_account",          "batch_transfer")
                                | ("account",                "create_account")
                            );

                            let is_singleton_resource =
                                |resource: &str, addr: &str| -> bool {
                                    if addr == "0xa" {
                                        return true;
                                    }
                                    // Removed addr == "0x1" requirement so it matches UNKNOWN addresses too
                                    if resource.contains("::coin::CoinConversionMap")
                                        || resource.contains("::features::Features")
                                        || resource.contains("::primary_fungible_store::DeriveRefPod")
                                        || resource.contains("::transaction_fee::AptosFABurnCapabilities")
                                        || resource.contains("::transaction_fee::CollectedFeesPerBlock")
                                        || resource.contains("::coin::PairedFungibleAssetRefs")
                                        || resource.contains("::coin::PairedCoinType")
                                        || resource.contains("::fungible_asset::Metadata")
                                        || resource.contains("::object::Untransferable")
                                    {
                                        return true;
                                    }
                                    // Aggregator-backed supply resources — increments are commutative deltas,
                                    // order-independent across txns. Demote writes to reads so DAG doesn't
                                    // serialize mints through the supply tracker.
                                    if resource.contains("::fungible_asset::ConcurrentSupply")
                                        || resource.contains("::fungible_asset::Supply")
                                        || resource.contains("::collection::ConcurrentSupply")
                                        || resource.contains("::collection::FixedSupply")
                                        || resource.contains("::collection::UnlimitedSupply")
                                    {
                                        return true;
                                    }

                                    // ---> ADD THIS NEW BLOCK <---
                                    // 2. SPURIOUS COIN INITIALIZATION
                                    // The static analyzer explores un-taken `coin::initialize` branches.
                                    // Demote these to reads so they don't falsely serialize the block.
                                    if resource.contains("::coin::CoinInfo") 
                                        || resource.contains("::coin::CoinConversionMap") 
                                        || resource.contains("::coin::PairedCoinType")
                                    {
                                        return true;
                                    }
                                    // ----------------------------
                                    
                                    false
                                };

                            for path in &symbolic_summary.read_set {
                                for addr in concretize(&path.address_source) {
                                    let is_unknown = addr.contains("UNKNOWN");
                                    let res_id  = intern_str(path.resource_type.clone());
                                    let addr_id = intern_str(addr);
                                    read_keys.push((res_id, addr_id, is_unknown));
                                }
                            }

                            for path in &symbolic_summary.write_set {
                                for addr in concretize(&path.address_source) {
                                    let is_unknown = addr.contains("UNKNOWN");
                                    
                                    // CRITICAL FIX: Removed `!is_unknown` constraint. 
                                    // Spurious Bottom writes to these resources are now safely demoted to blind reads.
                                    if !writes_singleton_resources
                                        && is_singleton_resource(&path.resource_type, &addr)
                                    {
                                        // Demote to read
                                        let res_id  = intern_str(path.resource_type.clone());
                                        let addr_id = intern_str(addr);
                                        // Push as false to prevent UNKNOWN logic triggering fallback HashMap overhead
                                        read_keys.push((res_id, addr_id, false));
                                    } else {
                                        let res_id  = intern_str(path.resource_type.clone());
                                        let addr_id = intern_str(addr);
                                        write_keys.push((res_id, addr_id, is_unknown));
                                    }
                                }
                            }
                            // Add a temporary eprintln for concrete diagnostics of txn 15, 16, 17, 18
                            if i >= 1 && i <= 4 {
                                eprintln!("Concrete diagnostics for txn {}: read_keys={:?}, write_keys={:?}", i, read_keys, write_keys);
                            }
                            concrete_cache.insert(cache_key, (read_keys.clone(), write_keys.clone()));
                        }
                    }
                }
                (read_keys, write_keys, is_analyzable)
            }).collect()
        };

        // -----------------------------------------------------------------
        // STEP 1A.5: Inject BlockMetadata write set + VM prologue reads.
        //
        // Problem: BlockMetadata (txn 0) is not a UserTransaction, so the
        // parallel analysis above produces an empty concrete set for it with
        // is_analyzable=false. This triggers the unconditional `last_barrier`
        // logic in STEP 1B, which adds an edge from txn 0 to *every* subsequent
        // txn — serialising the entire block on a single first txn even when
        // most user txns never touch BlockMetadata resources.
        //
        // Root cause of the earlier "Final Validation failed" crash: removing
        // the barrier naively caused user txns to execute before BlockMetadata
        // committed. Even coin_transfer reads timestamp/epoch via the VM-injected
        // transaction prologue — reads that do NOT appear in the symbolic summary
        // because the prologue is runtime code, not user bytecode.
        //
        // Fix (two parts, both required):
        //
        //   Part A — Mark concrete_sets[0] as analyzable and populate its write
        //   set with BlockMetadata's known stable writes. This lets STEP 1B's
        //   normal RAW tracker populate last_writers for those keys instead of
        //   setting last_barrier.
        //
        //   Part B — Append the prologue's implicit reads to every UserTransaction
        //   concrete set. STEP 1B's RAW check then finds last_writers[timestamp] = 0
        //   and emits a precise 0 → i edge for each user txn.
        //
        // Net result: initial_ready=1 (only BlockMetadata), then all ~N user txns
        // become simultaneously ready the instant BlockMetadata commits — maximum
        // parallelism with correct read-version ordering. The blunt last_barrier
        // fan-out is eliminated entirely for the common case.
        //
        // String format must match concretize(): short-form "0x1", not padded hex.
        // -----------------------------------------------------------------

        // Sequential intern helper — safe to call now that par_iter is done.
        // Uses the same intern_map and intern_counter as STEP 1A so all IDs
        // live in one consistent namespace.
        {
            // Sequential intern — same DashMap, just no parallelism needed.
            let intern_seq = |s: &str| -> u32 {
                let candidate = intern_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *intern_map.entry(s.to_string()).or_insert(candidate)
            };

            let addr_0x1  = intern_seq("0x1");
            let res_ts    = intern_seq("0x1::timestamp::CurrentTimeMicroseconds");
            let res_bm    = intern_seq("0x1::block::BlockMetadata");
            let res_reconf = intern_seq("0x1::reconfiguration::Configuration");
            let res_reconf_state = intern_seq("0x1::reconfiguration_state::State");

            // Patch txn 0: inject BlockMetadata's writes, mark analyzable.
            {
                let (_, writes, is_analyzable) = &mut concrete_sets[0];
                if !*is_analyzable {
                    *is_analyzable = true;
                    writes.push((res_ts,          addr_0x1, false));
                    writes.push((res_bm,          addr_0x1, false));
                    writes.push((res_reconf,      addr_0x1, false));
                    writes.push((res_reconf_state, addr_0x1, false));
                }
            }

            // Patch every user txn: append prologue implicit reads.
            for (reads, _, is_analyzable) in concrete_sets.iter_mut().skip(1) {
                if *is_analyzable {
                    reads.push((res_ts, addr_0x1, false));
                    reads.push((res_bm, addr_0x1, false));
                }
            }
            // eprintln!(
            //     "[DAG-DIAG] concrete_sets[0]: analyzable={}, writes={:?}",
            //     concrete_sets[0].2,
            //     concrete_sets[0].1
            // );
            // eprintln!(
            //     "[DAG-DIAG] concrete_sets[1] reads (first 4): {:?}",
            //     &concrete_sets[1].0[..concrete_sets[1].0.len().min(4)]
            // );
            // // ADD HERE:
            // eprintln!(
            //     "[DAG-DIAG] concrete_sets[1] writes: {:?}",
            //     concrete_sets[1].1
            // );
            // if concrete_sets.len() > 2 {
            // eprintln!(
            //         "[DAG-DIAG] concrete_sets[2] writes: {:?}",
            //         concrete_sets[2].1
            //     );
            // }
            // let mut intern_entries: Vec<_> = intern_map.iter()
            //     .map(|e| (*e.value(), e.key().clone()))
            //     .collect();
            // intern_entries.sort();
            // eprintln!(
            //     "[DAG-DIAG] intern_map (first 20): {:?}",
            //     &intern_entries[..intern_entries.len().min(20)]
            // );
        }

        // Check for publish transactions — skip DAG entirely if any exist.
        let is_publish_block = (0..num_txns).any(|i| {
            is_publish_transaction(&signature_verified_block.get_txn(i as u32))
        });

        let analysis_elapsed = analysis_start.elapsed();
        eprintln!(
            "[PARTITION-TIMING] static_analysis={}ms ({} txns, {}µs/txn, hits={} misses={})",
            analysis_elapsed.as_millis(),
            num_txns,
            analysis_elapsed.as_micros() / num_txns as u128,
            cache_hits.load(std::sync::atomic::Ordering::Relaxed),
            cache_misses.load(std::sync::atomic::Ordering::Relaxed),
        );
        let dag_construction_start = std::time::Instant::now();

        // STEP 1B: DAG CONSTRUCTION
        if !is_publish_block {
            let mut out_edges: Vec<Vec<usize>> =
                (0..num_txns).map(|_| Vec::new()).collect();
            let mut in_degrees = vec![0usize; num_txns];

            let new_object_res_ids: HashSet<u32> = [
                "0x1::object::ObjectCore",
                "0x1::object::ObjectCore@UNKNOWN",
                "0x1::object::TombStone",
                "0x1::object::TombStone@UNKNOWN",
                "0x4::token::Token",
                "0x4::token::Token@UNKNOWN",
                "0x4::token::TokenIdentifiers",
                "0x4::token::TokenIdentifiers@UNKNOWN",
                "0x4::royalty::Royalty",
                "0x4::royalty::Royalty@UNKNOWN",
                "0x4::collection::Collection",
                "0x4::collection::Collection@UNKNOWN",
            ]
            .iter()
            .filter_map(|s| intern_map.get(*s).map(|r| *r))
            .collect();
            let has_unknowns = concrete_sets.iter().any(|(r, w, _)| 
                r.iter().any(|x| x.2) || w.iter().any(|x| x.2)
            );

            // O(1) Edge Deduplication
            let mut add_edge = |out_edges: &mut Vec<Vec<usize>>, _in_degrees: &mut Vec<usize>, from: usize, to: usize| {
                if from != to {
                    out_edges[from].push(to);
                }
            };

            let total_reads: usize = concrete_sets.iter().map(|(r, _, _)| r.len()).sum();
            let total_writes: usize = concrete_sets.iter().map(|(_, w, _)| w.len()).sum();

            if !has_unknowns {
                // ====================================================================
                // 🚀 HOT PATH: O(N log N) SORT & SWEEP
                // ====================================================================
                
                #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
                struct Access {
                    res_id: u32,
                    addr_id: u32,
                    txn_idx: usize,
                    is_write: bool, 
                }

                let mut accesses = Vec::with_capacity(total_reads + total_writes);
                let mut last_barrier: Option<usize> = None;

                for i in 0..num_txns {
                    if let Some(b) = last_barrier { add_edge(&mut out_edges, &mut in_degrees, b, i); }
                    
                    let (reads, writes, is_analyzable) = &concrete_sets[i];
                    if !*is_analyzable {
                        // CRITICAL FIX: Barrier must wait for all prior txns in current epoch!
                        let start_idx = last_barrier.unwrap_or(0);
                        for prev in start_idx..i {
                            add_edge(&mut out_edges, &mut in_degrees, prev, i);
                        }
                        last_barrier = Some(i);
                        continue;
                    }
                    
                    for &(res_id, addr_id, _) in reads {
                        accesses.push(Access { res_id, addr_id, txn_idx: i, is_write: false });
                    }
                    for &(res_id, addr_id, _) in writes {
                        if !new_object_res_ids.contains(&res_id) {
                            accesses.push(Access { res_id, addr_id, txn_idx: i, is_write: true });
                        }
                    }
                }

                accesses.sort_unstable();

                let mut idx = 0;
                while idx < accesses.len() {
                    let current_res = accesses[idx].res_id;
                    let current_addr = accesses[idx].addr_id;
                    
                    let mut last_writer = None;
                    let mut current_readers = Vec::new();

                    while idx < accesses.len() && accesses[idx].res_id == current_res && accesses[idx].addr_id == current_addr {
                        let acc = accesses[idx];
                        
                        if acc.is_write {
                            if let Some(w) = last_writer {
                                add_edge(&mut out_edges, &mut in_degrees, w, acc.txn_idx);
                            }

                            // WAR is no longer an issue because of MVHashmap
                            // for &r in &current_readers {
                            //     add_edge(&mut out_edges, &mut in_degrees, r, acc.txn_idx);
                            // }
                            last_writer = Some(acc.txn_idx);
                            current_readers.clear(); 
                        } else {
                            if let Some(w) = last_writer {
                                add_edge(&mut out_edges, &mut in_degrees, w, acc.txn_idx);
                            }
                            if current_readers.last() != Some(&acc.txn_idx) {
                                current_readers.push(acc.txn_idx);
                            }
                        }
                        idx += 1;
                    }
                }
            } else {
                // ====================================================================
                // 🐢 COLD PATH: Fallback to HashMaps (Executes ONLY if unknowns exist)
                // ====================================================================
                let mut last_writers:           ahash::AHashMap<(u32, u32), usize> = ahash::AHashMap::with_capacity(total_writes);
                // CRITICAL FIX: Readers are now accumulated safely in Vecs
                let mut last_readers:           ahash::AHashMap<(u32, u32), Vec<usize>> = ahash::AHashMap::with_capacity(total_reads);
                let mut last_unknown_writers:   ahash::AHashMap<u32, usize> = ahash::AHashMap::with_capacity(512);
                let mut last_unknown_readers:   ahash::AHashMap<u32, Vec<usize>> = ahash::AHashMap::with_capacity(512);
                let mut known_resource_writers: ahash::AHashMap<u32, Vec<usize>> = ahash::AHashMap::with_capacity(512);
                let mut known_resource_readers: ahash::AHashMap<u32, Vec<usize>> = ahash::AHashMap::with_capacity(512);

                let mut last_barrier: Option<usize> = None;

                for i in 0..num_txns {
                    if let Some(b) = last_barrier { add_edge(&mut out_edges, &mut in_degrees, b, i); }

                    let (reads, writes, is_analyzable) = &concrete_sets[i];
                    if !*is_analyzable {
                        // CRITICAL FIX: Barrier must wait for all prior txns in current epoch!
                        let start_idx = last_barrier.unwrap_or(0);
                        for prev in start_idx..i {
                            add_edge(&mut out_edges, &mut in_degrees, prev, i);
                        }
                        last_barrier = Some(i);
                        
                        last_writers.clear();
                        last_readers.clear();
                        last_unknown_writers.clear();
                        last_unknown_readers.clear();
                        known_resource_writers.clear();
                        known_resource_readers.clear();
                        continue;
                    }

                    // 1. RAW dependencies
                    for &(res_id, addr_id, is_unknown) in reads {
                        if is_unknown {
                            if let Some(&w) = last_unknown_writers.get(&res_id) { add_edge(&mut out_edges, &mut in_degrees, w, i); }
                            if !new_object_res_ids.contains(&res_id) {
                                if let Some(writers) = known_resource_writers.get(&res_id) {
                                    for &w in writers { add_edge(&mut out_edges, &mut in_degrees, w, i); }
                                }
                            }
                        } else {
                            if let Some(&w) = last_writers.get(&(res_id, addr_id)) { add_edge(&mut out_edges, &mut in_degrees, w, i); }
                            if let Some(&w) = last_unknown_writers.get(&res_id) { add_edge(&mut out_edges, &mut in_degrees, w, i); }
                        }
                    }

                    // 2. WAW and WAR dependencies
                    for &(res_id, addr_id, is_unknown) in writes {
                        // CRITICAL FIX: Ignore all writes to dynamically allocated new object resources, 
                        // EVEN if they are UNKNOWN. NFT mints generate UNKNOWN ObjectCore addresses 
                        // which falsely serialize the entire block if tracked.
                        if new_object_res_ids.contains(&res_id) { continue; }
                        
                        if is_unknown {
                            if let Some(&lw) = last_unknown_writers.get(&res_id) { add_edge(&mut out_edges, &mut in_degrees, lw, i); }
                            
                            // WAR are no longer an issue because of MVHashmap
                            // if let Some(readers) = last_unknown_readers.get(&res_id) {
                            //     for &r in readers { add_edge(&mut out_edges, &mut in_degrees, r, i); }
                            // }
                            if let Some(writers) = known_resource_writers.get(&res_id) {
                                for &w in writers { add_edge(&mut out_edges, &mut in_degrees, w, i); }
                            }
                            
                            // WAR are no longer an issue because of MVHashmap
                            // if let Some(readers) = known_resource_readers.get(&res_id) {
                            //     for &r in readers { add_edge(&mut out_edges, &mut in_degrees, r, i); }
                            // }
                        } else {
                            if let Some(&lw) = last_writers.get(&(res_id, addr_id)) { add_edge(&mut out_edges, &mut in_degrees, lw, i); }

                            // WAR are no longer an issue because of MVHashmap
                            // if let Some(readers) = last_readers.get(&(res_id, addr_id)) {
                            //     for &r in readers { add_edge(&mut out_edges, &mut in_degrees, r, i); }
                            // }
                            if let Some(&lw) = last_unknown_writers.get(&res_id) { add_edge(&mut out_edges, &mut in_degrees, lw, i); }

                            // WAR are no longer an issue because of MVHashmap
                            // if let Some(readers) = last_unknown_readers.get(&res_id) {
                            //     for &r in readers { add_edge(&mut out_edges, &mut in_degrees, r, i); }
                            // }
                        }
                    }

                    // 3. Update trackers
                    for &(res_id, addr_id, is_unknown) in reads {
                        if is_unknown {
                            last_unknown_readers.entry(res_id).or_default().push(i);
                        } else {
                            last_readers.entry((res_id, addr_id)).or_default().push(i);
                            if !new_object_res_ids.contains(&res_id) {
                                known_resource_readers.entry(res_id).or_default().push(i);
                            }
                        }
                    }
                    for &(res_id, addr_id, is_unknown) in writes {
                        if new_object_res_ids.contains(&res_id) { continue; }
                        
                        if is_unknown {
                            last_unknown_writers.insert(res_id, i);
                            last_unknown_readers.remove(&res_id);
                            known_resource_readers.remove(&res_id);
                            known_resource_writers.remove(&res_id);
                        } else {
                            last_writers.insert((res_id, addr_id), i);
                            last_readers.remove(&(res_id, addr_id));
                            known_resource_writers.entry(res_id).or_default().push(i);
                        }
                    }
                }
            }
            // Post-process: Sort, deduplicate, and calculate accurate in-degrees for the whole graph
            for from in 0..num_txns {
                let edges = &mut out_edges[from];
                edges.sort_unstable();
                edges.dedup();
                for &to in edges.iter() {
                    in_degrees[to] += 1;
                }
            }

            // STEP 1C: WAVE SCHEDULING
            // Clone before BFS since BFS destructively decrements in_degrees.
            let final_out_edges: Vec<Vec<usize>> = out_edges.clone();

            // TEMP DIAGNOSTIC FOR CUSTOM DAG SHAPE
            // println!("\n=== VERIFYING BINARY TREE EDGES (FIRST 10 TXNS) ===");
            // for i in 0..std::cmp::min(10, num_txns) {
            //     println!("Txn {} triggers -> {:?}", i, final_out_edges[i]);
            // }
            // println!("===================================================\n");

            let final_in_degrees: Vec<usize>     = in_degrees.clone();

            let mut computed_waves = Vec::new();
            let mut current_wave: Vec<usize> =
                (0..num_txns).filter(|&i| in_degrees[i] == 0).collect();
            while !current_wave.is_empty() {
                computed_waves.push(current_wave.clone());
                let mut next_wave = Vec::new();
                for &node in &current_wave {
                    for &child in &out_edges[node] {
                        in_degrees[child] -= 1;
                        if in_degrees[child] == 0 {
                            next_wave.push(child);
                        }
                    }
                }
                current_wave = next_wave;
            }

            dag_log!("--- DEPENDENCY GRAPH ---");
            for i in 0..num_txns {
                if !final_out_edges[i].is_empty() {
                    dag_log!("  Txn {} -> {:?}", i, final_out_edges[i]);
                }
            }
            dag_log!("--- SCHEDULED WAVES ---");
            for (w_idx, wave) in computed_waves.iter().enumerate() {
                dag_log!("  Wave {}: {:?}", w_idx, wave);
            }
            dag_log!("--------------------------------------");

            let dag_construction_elapsed = dag_construction_start.elapsed();
            eprintln!(
                "[PARTITION-TIMING] dag_construction={}ms ({} waves, {} edges, {}µs/txn)",
                dag_construction_elapsed.as_millis(),
                computed_waves.len(),
                final_out_edges.iter().map(|v| v.len()).sum::<usize>(),
                dag_construction_elapsed.as_micros() / num_txns as u128
            );

            // DIAGNOSTIC — remove after investigation
            {
                let unknown_read_count: usize = concrete_sets.iter()
                    .map(|(reads, _, _)| reads.iter().filter(|&&(_, _, u)| u).count())
                    .sum();
                let unknown_write_count: usize = concrete_sets.iter()
                    .map(|(_, writes, _)| writes.iter().filter(|&&(_, _, u)| u).count())
                    .sum();
                let wave_sizes: Vec<usize> = computed_waves.iter().map(|w| w.len()).collect();
                eprintln!(
                    "[DAG-DIAG] unknown_reads={} unknown_writes={} waves={} wave_sizes={:?}",
                    unknown_read_count, unknown_write_count, computed_waves.len(), &wave_sizes[..wave_sizes.len().min(10)]
                );
                // Print the most-connected nodes to identify false-chain sources
                let mut edge_counts: Vec<(usize, usize)> = final_out_edges.iter()
                    .enumerate()
                    .map(|(i, e)| (i, e.len()))
                    .collect();
                edge_counts.sort_by(|a, b| b.1.cmp(&a.1));
                eprintln!("[DAG-DIAG] top-5 out-degree nodes: {:?}", &edge_counts[..edge_counts.len().min(5)]);
            }

            // Add after the existing top-5 diagnostic
            let mut degree_hist: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
            for edges in &final_out_edges {
                *degree_hist.entry(edges.len()).or_insert(0) += 1;
            }
            let mut hist_sorted: Vec<_> = degree_hist.into_iter().collect();
            hist_sorted.sort();
            eprintln!("[DAG-DIAG] out-degree histogram: {:?}", &hist_sorted[..hist_sorted.len().min(8)]);

            // And in-degree histogram
            let mut in_hist: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
            for &d in &final_in_degrees {
                *in_hist.entry(d).or_insert(0) += 1;
            }
            let mut in_sorted: Vec<_> = in_hist.into_iter().collect();
            in_sorted.sort();
            eprintln!("[DAG-DIAG] in-degree histogram: {:?}", &in_sorted[..in_sorted.len().min(8)]);

            waves = Some((computed_waves, final_out_edges, final_in_degrees));
        } else {
            dag_log!("Skipped due to Publish Transaction");
        }
        eprintln!(
            "[PARTITION-TIMING] total_partition={}ms (analysis + construction)",
            dag_start.elapsed().as_millis()
        );
        // print intern_map
        {
            let mut intern_entries: Vec<_> = intern_map.iter()
                .map(|e| (*e.value(), e.key().clone()))
                .collect();
            intern_entries.sort();
            eprintln!(
                "[DAG-DIAG] intern_map (first 100): {:?}",
                &intern_entries[..intern_entries.len().min(100)]
            );
        }
    }

    let executor = BlockExecutor::<
        SignatureVerifiedTransaction,
        E,
        S,
        L,
        TP,
        AuxiliaryInfo,
    >::new(config, executor_thread_pool, transaction_commit_listener);

    let ret = executor.execute_block(
        signature_verified_block,
        state_view,
        &transaction_slice_metadata,
        &mut module_cache_manager_guard,
        waves,
    );

    match ret {
        Ok(block_output) => {
            let (transaction_outputs, block_epilogue_txn) = block_output.into_inner();
            let output_vec: Vec<_> = transaction_outputs
                .into_iter()
                .map(|output| output.take_output())
                .collect();
            let pos = output_vec.partition_point(|o| !o.status().is_retry());
            if state_view.id() != StateViewId::Miscellaneous {
                flush_speculative_logs(pos);
            }
            Ok(BlockOutput::new(output_vec, block_epilogue_txn))
        },
        Err(BlockExecutionError::FatalBlockExecutorError(PanicError::CodeInvariantError(
            err_msg,
        ))) => Err(VMStatus::Error {
            status_code:
                StatusCode::DELAYED_FIELD_OR_BLOCKSTM_CODE_INVARIANT_ERROR,
            sub_status: None,
            message: Some(err_msg),
        }),
        Err(BlockExecutionError::FatalVMError(err)) => Err(err),
    }
}

    /// Uses shared thread pool to execute blocks.
    pub(crate) fn execute_block<
        S: StateView + Sync,
        L: TransactionCommitHook,
        TP: TxnProvider<SignatureVerifiedTransaction, AuxiliaryInfo> + Sync,
    >(
        signature_verified_block: &TP,
        state_view: &S,
        module_cache_manager: &AptosModuleCacheManager,
        config: BlockExecutorConfig,
        transaction_slice_metadata: TransactionSliceMetadata,
        transaction_commit_listener: Option<L>,
    ) -> Result<BlockOutput<SignatureVerifiedTransaction, TransactionOutput>, VMStatus> {
        let num_threads = std::cmp::min(config.local.concurrency_level, num_cpus::get());
        let pool = match &mut *RAYON_EXEC_POOL.lock().unwrap() {
            Some((pool, n)) if *n == num_threads => Arc::clone(pool),
            slot => {
                info!(num_threads = num_threads, "Creating par_exec thread pool");
                let pool = Arc::new(
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(num_threads)
                        .thread_name(|index| format!("par_exec-{}", index))
                        .build()
                        .unwrap(),
                );
                *slot = Some((Arc::clone(&pool), num_threads));
                pool
            },
        };
        Self::execute_block_on_thread_pool::<S, L, TP>(
            pool,
            signature_verified_block,
            state_view,
            module_cache_manager,
            config,
            transaction_slice_metadata,
            transaction_commit_listener,
        )
    }
}

// Same as AptosBlockExecutorWrapper with AptosExecutorTask
pub type AptosVMBlockExecutorWrapper = AptosBlockExecutorWrapper<AptosExecutorTask>;