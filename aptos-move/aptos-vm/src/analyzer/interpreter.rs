use std::collections::HashMap;
use dashmap::DashMap;
use move_binary_format::CompiledModule;
use move_binary_format::file_format::{Bytecode, FunctionDefinition};
use move_binary_format::access::ModuleAccess; 

use crate::analyzer::abstract_domain::{AbstractValue, Provenance, FunctionSummary, AccessPath, SideEffect};
use crate::analyzer::FunctionID;

pub struct Interpreter<'a> {
    module: &'a CompiledModule,
    global_summaries: &'a DashMap<FunctionID, FunctionSummary>,
    stack: Vec<AbstractValue>,
    locals: HashMap<usize, AbstractValue>,
    summary: FunctionSummary,
    current_func_def: Option<&'a FunctionDefinition>,
}

impl<'a> Interpreter<'a> {
    pub fn new(module: &'a CompiledModule, global_summaries: &'a DashMap<FunctionID, FunctionSummary>) -> Self {
        Self {
            module,
            global_summaries,
            stack: Vec::new(),
            locals: HashMap::new(),
            summary: FunctionSummary::new(),
            current_func_def: None,
        }
    }

    pub fn analyze_function(&mut self, func_def: &'a FunctionDefinition) -> FunctionSummary {
        self.current_func_def = Some(func_def);
        self.initialize_arguments(func_def);

        let handle = self.module.function_handle_at(func_def.function);
        let name = self.module.identifier_at(handle.name).to_string();
        // eprintln!("[ANALYZE] analyzing function: {}", name);

        if let Some(code_unit) = &func_def.code {
            self.analyze_with_cfg(&code_unit.code);
        }

        self.summary.clone()
    }

    /// Worklist-based abstract interpretation over the function's control-flow graph.
    ///
    /// Move bytecode has explicit branch instructions (`BrTrue`, `BrFalse`, `Branch`) with
    /// absolute offsets. A linear scan ignores these entirely: it never visits branch targets
    /// as merge points, so any function with conditional logic produces wrong results because
    /// the abstract stack state at a join point is never the union of both incoming paths.
    ///
    /// Algorithm:
    ///   1. Identify basic block boundaries: every branch target and every instruction
    ///      immediately following a branch/abort starts a new block.
    ///   2. Build a successor map: block_start -> [successor_block_starts].
    ///   3. Run a worklist from block 0. For each block, execute instructions linearly from
    ///      the block's entry stack state, updating `self.summary` for any storage accesses
    ///      encountered. On block exit, merge the outgoing stack into each successor's entry.
    ///   4. Merge (join) is conservative: stacks must have the same depth (Move is stack-typed);
    ///      each slot keeps its provenance only if both sides agree; otherwise it becomes Bottom.
    ///      If a successor's entry state changes, re-add it to the worklist.
    ///   5. Repeat until stable (guaranteed to terminate: Bottom is the only direction we can
    ///      move, and the provenance lattice is finite for any given function).
    fn analyze_with_cfg(&mut self, instructions: &[Bytecode]) {
        let n = instructions.len();
        if n == 0 { return; }

        // ----------------------------------------------------------------
        // Step 1: Identify block leaders.
        // ----------------------------------------------------------------
        let mut leaders: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        leaders.insert(0);

        for (pc, instr) in instructions.iter().enumerate() {
            match instr {
                Bytecode::Branch(target) => {
                    leaders.insert(*target as usize);
                    if pc + 1 < n { leaders.insert(pc + 1); }
                },
                Bytecode::BrTrue(target) | Bytecode::BrFalse(target) => {
                    leaders.insert(*target as usize);
                    if pc + 1 < n { leaders.insert(pc + 1); }
                },
                Bytecode::Ret | Bytecode::Abort | Bytecode::AbortMsg => {
                    if pc + 1 < n { leaders.insert(pc + 1); }
                },
                _ => {},
            }
        }

        let leaders_vec: Vec<usize> = leaders.iter().cloned().collect();

        // ----------------------------------------------------------------
        // Step 2: Build successor map: block_start -> Vec<block_start>.
        // ----------------------------------------------------------------
        let mut successors: HashMap<usize, Vec<usize>> = HashMap::new();
        for &leader in &leaders_vec {
            let next_leader = leaders_vec.iter().find(|&&l| l > leader).cloned().unwrap_or(n);
            let block_instrs = &instructions[leader..next_leader];
            let mut succs = Vec::new();
            match block_instrs.last() {
                Some(Bytecode::Branch(target)) => {
                    succs.push(*target as usize);
                },
                Some(Bytecode::BrTrue(target)) | Some(Bytecode::BrFalse(target)) => {
                    // Conditional branch: taken edge and fall-through edge.
                    succs.push(*target as usize);
                    if next_leader < n { succs.push(next_leader); }
                },
                Some(Bytecode::Ret) | Some(Bytecode::Abort) | Some(Bytecode::AbortMsg) => {
                    // Terminal — no successors.
                },
                _ => {
                    // Fall-through to the next block.
                    if next_leader < n { succs.push(next_leader); }
                },
            }
            successors.insert(leader, succs);
        }

        // ----------------------------------------------------------------
        // Step 3 & 4: Worklist iteration.
        // ----------------------------------------------------------------
        // Each block's entry state is (stack, locals). Both must be snapshotted and
        // joined at merge points — storing only the stack was wrong because StLoc
        // mutates `self.locals` during block execution, so on a re-visit the locals
        // already contain values written on a previous pass from a different predecessor.
        type BlockState = (Vec<AbstractValue>, HashMap<usize, AbstractValue>);
        let mut block_entry: HashMap<usize, BlockState> = HashMap::new();

        // Block 0: empty stack, locals seeded with function arguments by initialize_arguments.
        block_entry.insert(0, (Vec::new(), self.locals.clone()));

        let mut worklist: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        worklist.push_back(0);

        while let Some(block_start) = worklist.pop_front() {
            let (entry_stack, entry_locals) = match block_entry.get(&block_start) {
                Some(s) => s.clone(),
                None => continue,
            };

            // Restore both stack and locals to this block's entry state before executing.
            self.stack = entry_stack;
            self.locals = entry_locals;

            let next_leader = leaders_vec.iter().find(|&&l| l > block_start).cloned().unwrap_or(n);
            for instr in &instructions[block_start..next_leader] {
                self.execute_instruction(instr);
            }
            let exit_stack = self.stack.clone();
            let exit_locals = self.locals.clone();

            // Propagate the exit state to each successor, joining if already visited.
            if let Some(succs) = successors.get(&block_start).cloned() {
                for succ in succs {
                    let changed = match block_entry.get(&succ) {
                        None => {
                            block_entry.insert(succ, (exit_stack.clone(), exit_locals.clone()));
                            true
                        },
                        Some((existing_stack, existing_locals)) => {
                            let merged_stack = Self::join_stacks(existing_stack, &exit_stack);
                            let merged_locals = Self::join_locals(existing_locals, &exit_locals);
                            let stack_changed = merged_stack != *existing_stack;
                            let locals_changed = merged_locals != *existing_locals;
                            if stack_changed || locals_changed {
                                block_entry.insert(succ, (merged_stack, merged_locals));
                                true
                            } else {
                                false
                            }
                        }
                    };
                    if changed {
                        worklist.push_back(succ);
                    }
                }
            }
        }
    }

    /// Conservative join of two provenances at a control-flow merge point.
    ///
    /// If both paths agree on the exact same provenance, that provenance is preserved.
    /// Any disagreement becomes Bottom: we cannot know at analysis time which branch the
    /// VM will take, so we must assume the worst (no pointer information).
    fn join_provenance(a: &Provenance, b: &Provenance) -> Provenance {
        if a == b { a.clone() } else { Provenance::Bottom }
    }

    /// Conservative join of two abstract stacks at a control-flow merge point.
    ///
    /// Move's type system guarantees both incoming paths leave the stack at the same depth,
    /// so a depth mismatch signals a bug in block construction rather than user code — we
    /// take the shorter of the two stacks defensively.
    fn join_stacks(a: &[AbstractValue], b: &[AbstractValue]) -> Vec<AbstractValue> {
        a.iter().zip(b.iter()).map(|(va, vb)| {
            AbstractValue {
                provenance: Self::join_provenance(&va.provenance, &vb.provenance),
            }
        }).collect()
    }

    /// Conservative join of two locals maps at a control-flow merge point.
    ///
    /// Only slots present in both maps are kept; a slot present in only one predecessor
    /// means the other path never wrote it, so we cannot rely on its value — it becomes
    /// Bottom. Slots present in both are joined slot-wise with join_provenance.
    fn join_locals(
        a: &HashMap<usize, AbstractValue>,
        b: &HashMap<usize, AbstractValue>,
    ) -> HashMap<usize, AbstractValue> {
        let mut result = HashMap::new();
        for (idx, va) in a {
            let prov = if let Some(vb) = b.get(idx) {
                Self::join_provenance(&va.provenance, &vb.provenance)
            } else {
                // Present in `a` but not `b`: the other path never initialised this slot.
                Provenance::Bottom
            };
            result.insert(*idx, AbstractValue {
                provenance: prov,
            });
        }
        // Slots only in `b` also become Bottom (same reasoning, symmetric).
        for (idx, vb) in b {
            if !a.contains_key(idx) {
                result.insert(*idx, AbstractValue {
                    provenance: Provenance::Bottom,
                });
            }
        }
        result
    }

    fn initialize_arguments(&mut self, func_def: &FunctionDefinition) {
        let handle = self.module.function_handle_at(func_def.function);
        let params_sig_idx = handle.parameters;
        let params = &self.module.signature_at(params_sig_idx).0;

        for (i, param_type) in params.iter().enumerate() {
            let abs_val = AbstractValue {
                provenance: Provenance::Param(i),
            };
            self.locals.insert(i, abs_val);
        }
    }

    fn execute_instruction(&mut self, instruction: &Bytecode) {
        match instruction {
            // --- MOVEMENT ---
            Bytecode::CopyLoc(idx) | Bytecode::MoveLoc(idx) => {
                if let Some(val) = self.locals.get(&(*idx as usize)) {
                    self.stack.push(val.clone());
                } else {
                    self.stack.push(AbstractValue::new_bottom());
                }
            },
            Bytecode::StLoc(idx) => {
                if let Some(val) = self.stack.pop() {
                    self.locals.insert(*idx as usize, val);
                }
            },

            // The reference inherits the provenance of the local variable.
            // If Local(1) holds Param(0), then &Local(1) also points to Param(0).
            Bytecode::ImmBorrowLoc(idx) | Bytecode::MutBorrowLoc(idx) => {
                if let Some(val) = self.locals.get(&(*idx as usize)) {
                    self.stack.push(AbstractValue {
                        provenance: val.provenance.clone(),
                    });
                } else {
                    self.stack.push(AbstractValue::new_bottom());
                }
            },

            // --- CONSTANTS ---
            Bytecode::LdConst(idx) => {
                let constant = self.module.constant_at(*idx);
                let data = &constant.data;
                let const_str = if data.len() == 32 {
                    if let Ok(a) = move_core_types::account_address::AccountAddress::from_bytes(data) {
                        a.to_hex_literal()
                    } else {
                        format!("0x{}", hex::encode(data))
                    }
                } else {
                    hex::encode(data)
                };
                self.stack.push(AbstractValue {
                    provenance: Provenance::Const(const_str),
                });
            },
            Bytecode::LdU64(_) | Bytecode::LdU8(_) | Bytecode::LdU128(_) | Bytecode::LdU16(_) | 
            Bytecode::LdU32(_) | Bytecode::LdU256(_) | Bytecode::LdTrue | Bytecode::LdFalse => {
                self.stack.push(AbstractValue {
                    provenance: Provenance::Bottom,
                });
            },

            // --- REFERENCES ---
            Bytecode::ReadRef => {
                if let Some(ref_val) = self.stack.pop() {
                    // Propagate the source. If we read &Param(0), the value is still derived from Param(0).
                    self.stack.push(AbstractValue {
                        provenance: ref_val.provenance,
                    });
                }
            },
            Bytecode::FreezeRef => {
                // No-op: &mut T and &T share the same provenance for our purposes.
            },
            Bytecode::WriteRef => {
                let ref_val = self.stack.pop(); // 1. Pop Reference being written TO (Top of Stack)
                let _val = self.stack.pop();    // 2. Pop Value being written
                if let Some(r) = ref_val {
                    let func_name = self.current_func_def.map(|def| {
                        let handle = self.module.function_handle_at(def.function);
                        self.module.identifier_at(handle.name).as_str()
                    }).unwrap_or("unknown_func");

                    eprintln!(
                        "[WRITEREF] {}::{} target={:?}",
                        self.module.self_id().name(),
                        func_name,
                        r.provenance
                    );
                    self.handle_write_to_provenance(&r.provenance);
                }
            },

            // --- GLOBAL STORAGE ---

            // ImmBorrowGlobal: acquires a shared reference — this is a READ.
            // MutBorrowGlobal: acquires an exclusive reference — this is also only a READ
            // at borrow time. A write is only committed when WriteRef is subsequently called
            // on the resulting reference. Recording the borrow itself as a write was wrong:
            // it inflated the write set and created false dependencies between transactions
            // that borrow-mut but never actually modify the resource.
            Bytecode::ImmBorrowGlobal(idx) | Bytecode::MutBorrowGlobal(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let struct_name = self.resolve_struct_name_from_def(*idx);

                    // Record a READ for every root address this operand may represent.
                    // (A struct address can carry multiple root pointers after the flatten fix.)
                    for flat_source in addr_val.provenance.flatten() {
                        self.summary.read_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }

                    // Push the reference with the full (unflattened) provenance so that
                    // downstream WriteRef / BorrowField instructions retain the struct layout.
                    self.stack.push(AbstractValue {
                        provenance: Provenance::Global {
                            resource_type: struct_name,
                            address_source: Box::new(addr_val.provenance),
                        },
                    });
                }
            },

            // Exists<T>(addr) -> bool: only reads T at addr
            Bytecode::Exists(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let struct_name = self.resolve_struct_name_from_def(*idx);
                    for flat_source in addr_val.provenance.flatten() {
                        self.summary.read_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }
                    self.stack.push(AbstractValue::new_bottom());
                }
            },

            // MoveFrom<T>(addr) -> T: removes T from global storage (a write/delete)
            Bytecode::MoveFrom(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let struct_name = self.resolve_struct_name_from_def(*idx);
                    let flat_sources = addr_val.provenance.flatten();
                    for flat_source in &flat_sources {
                        self.summary.write_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source.clone(),
                        });
                    }
                    // MoveFrom returns the moved value T. Use the first resolved root as the
                    // canonical provenance; a MoveFrom address is always a single pointer.
                    let canonical = flat_sources.into_iter().next().unwrap_or(Provenance::Bottom);
                    self.stack.push(AbstractValue {
                        provenance: Provenance::Global {
                            resource_type: struct_name,
                            address_source: Box::new(canonical),
                        },
                    });
                }
            },

            // MoveTo<T>(&signer, T): publishes T under the signer's address (a write)
            Bytecode::MoveTo(idx) => {
                let _resource_val = self.stack.pop(); 
                if let Some(signer_ref) = self.stack.pop() {
                    let struct_name = self.resolve_struct_name_from_def(*idx);
                    for flat_source in signer_ref.provenance.flatten() {
                        self.summary.write_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }
                }
            },

            // --- FIELDS ---
            Bytecode::ImmBorrowField(idx) | Bytecode::MutBorrowField(idx) => {
                if let Some(parent_val) = self.stack.pop() {
                    eprintln!(
                        "[MBF-IN ] parent={:?}",
                        parent_val.provenance
                    );
                    let field_handle = self.module.field_handle_at(*idx);
                    let field_idx = field_handle.field as usize;
                    
                    let new_prov = match &parent_val.provenance {
                        Provenance::Struct(fields) if field_idx < fields.len() => {
                            fields[field_idx].clone()
                        },
                        _ => Provenance::Field(Box::new(parent_val.provenance.clone()))
                    };

                    eprintln!(
                        "[MBF-OUT] result={:?}",
                        new_prov
                    );

                    self.stack.push(AbstractValue {
                        provenance: new_prov,
                    });
                }
            },

            // --- CALLS ---
            Bytecode::Call(idx) => self.execute_call(*idx),
            Bytecode::CallGeneric(idx) => {
                let inst = self.module.function_instantiation_at(*idx);
                self.execute_call(inst.handle);
            },
            Bytecode::Ret => {
                if let Some(def) = self.current_func_def {
                    let handle = self.module.function_handle_at(def.function);
                    let return_count = self.module.signature_at(handle.return_).0.len();
                    for i in (0..return_count).rev() {
                        if let Some(val) = self.stack.pop() {
                            // A function may have multiple Ret instructions (one per branch).
                            // Blindly inserting overwrites the provenance recorded by a previous
                            // Ret from a different path, discarding that path's information.
                            // Instead, join: if both paths agree on the same provenance, keep
                            // it; otherwise the return value's origin is ambiguous → Bottom.
                            let incoming = val.provenance;
                            let joined = match self.summary.return_dependencies.get(&i) {
                                None => incoming, // first Ret seen for this return slot
                                Some(existing) => Self::join_provenance(existing, &incoming),
                            };
                            if !matches!(joined, Provenance::Bottom) {
                                self.summary.return_dependencies.insert(i, joined);
                            } else {
                                // Explicitly remove any previously optimistic entry so callers
                                // don't see a stale provenance from only one branch.
                                self.summary.return_dependencies.remove(&i);
                            }
                        }
                    }
                }
            },

            // --- PACK / UNPACK ---
            Bytecode::Pack(idx) => {
                let count = self.get_field_count_def(*idx);
                self.execute_pack(count);
            },
            Bytecode::PackGeneric(idx) => {
                let count = self.get_field_count_inst(*idx);
                self.execute_pack(count);
            },
            Bytecode::Unpack(idx) => {
                let count = self.get_field_count_def(*idx);
                self.execute_unpack(count);
            },
            Bytecode::UnpackGeneric(idx) => {
                let count = self.get_field_count_inst(*idx);
                self.execute_unpack(count);
            },

            // --- VECTOR ---
            Bytecode::VecPack(_, count) => {
                // Collect provenance from ALL elements, not just the first non-Bottom one.
                // A vector may be used as an address container (e.g. object derivation), in
                // which case every element's provenance matters. We represent the vector as a
                // Struct of its elements so that flatten() and BorrowField can recover any root.
                let mut elem_provs: Vec<Provenance> = Vec::with_capacity(*count as usize);
                for _ in 0..*count {
                    let prov = self.stack.pop()
                        .map(|v| v.provenance)
                        .unwrap_or(Provenance::Bottom);
                    elem_provs.push(prov);
                }
                elem_provs.reverse(); // stack is LIFO; restore original order

                self.stack.push(AbstractValue {
                    provenance: Provenance::Struct(elem_provs),
                });
            },
            // --- NATIVE VECTOR BYTECODES ---
            Bytecode::VecImmBorrow(_) | Bytecode::VecMutBorrow(_) => {
                self.stack.pop(); // Pop the loop index
                if let Some(vec_val) = self.stack.pop() { // Pop the vector reference
                    self.stack.push(AbstractValue {
                        // UNIVERSAL TRICK: The single element inherits the provenance of the ENTIRE array!
                        provenance: vec_val.provenance, 
                    });
                } else {
                    self.stack.push(AbstractValue::new_bottom());
                }
            },
            Bytecode::VecPushBack(_) => {
                let _val = self.stack.pop(); // Pop value
                if let Some(vec_ref) = self.stack.pop() { // Pop vector ref
                    self.handle_write_to_provenance(&vec_ref.provenance); // Record the write!
                }
            },
            Bytecode::VecPopBack(_) => {
                if let Some(vec_val) = self.stack.pop() {
                    self.handle_write_to_provenance(&vec_val.provenance); // Record the write!
                    self.stack.push(AbstractValue {
                        provenance: vec_val.provenance, 
                    });
                } else {
                    self.stack.push(AbstractValue::new_bottom());
                }
            },
            Bytecode::VecUnpack(_, count) => {
                if let Some(vec_val) = self.stack.pop() {
                    for _ in 0..*count {
                        self.stack.push(AbstractValue {
                            provenance: vec_val.provenance.clone(),
                        });
                    }
                } else {
                    for _ in 0..*count { self.stack.push(AbstractValue::new_bottom()); }
                }
            },
            Bytecode::VecSwap(_) => {
                self.stack.pop(); // index 2
                self.stack.pop(); // index 1
                if let Some(vec_ref) = self.stack.pop() { // vector ref
                    self.handle_write_to_provenance(&vec_ref.provenance); // Record the write!
                }
            },
            Bytecode::VecLen(_) => {
                self.stack.pop(); // Pop vector reference
                self.stack.push(AbstractValue::new_bottom()); // Push u64 length
            },

            // --- GENERIC GLOBAL STORAGE ---
            Bytecode::ImmBorrowGlobalGeneric(idx) | Bytecode::MutBorrowGlobalGeneric(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let inst = self.module.struct_instantiation_at(*idx);
                    // Use the base struct name (e.g., 0x1::coin::CoinStore)
                    let struct_name = self.resolve_struct_name_from_def(inst.def);
                    
                    for flat_source in addr_val.provenance.flatten() {
                        self.summary.read_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }
                    self.stack.push(AbstractValue {
                        provenance: Provenance::Global {
                            resource_type: struct_name,
                            address_source: Box::new(addr_val.provenance),
                        },
                    });
                }
            },
            Bytecode::ExistsGeneric(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let inst = self.module.struct_instantiation_at(*idx);
                    let struct_name = self.resolve_struct_name_from_def(inst.def);
                    for flat_source in addr_val.provenance.flatten() {
                        self.summary.read_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }
                    self.stack.push(AbstractValue::new_bottom());
                }
            },
            Bytecode::MoveFromGeneric(idx) => {
                if let Some(addr_val) = self.stack.pop() {
                    let inst = self.module.struct_instantiation_at(*idx);
                    let struct_name = self.resolve_struct_name_from_def(inst.def);
                    let flat_sources = addr_val.provenance.flatten();
                    for flat_source in &flat_sources {
                        self.summary.write_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source.clone(),
                        });
                    }
                    let canonical = flat_sources.into_iter().next().unwrap_or(Provenance::Bottom);
                    self.stack.push(AbstractValue {
                        provenance: Provenance::Global {
                            resource_type: struct_name,
                            address_source: Box::new(canonical),
                        },
                    });
                }
            },
            Bytecode::MoveToGeneric(idx) => {
                let _resource_val = self.stack.pop(); 
                if let Some(signer_ref) = self.stack.pop() {
                    let inst = self.module.struct_instantiation_at(*idx);
                    let struct_name = self.resolve_struct_name_from_def(inst.def);
                    for flat_source in signer_ref.provenance.flatten() {
                        self.summary.write_set.insert(AccessPath {
                            resource_type: struct_name.clone(),
                            address_source: flat_source,
                        });
                    }
                }
            },

            // --- GENERIC FIELDS ---
            Bytecode::ImmBorrowFieldGeneric(idx) | Bytecode::MutBorrowFieldGeneric(idx) => {
                if let Some(parent_val) = self.stack.pop() {
                    eprintln!(
                        "[MBFG-IN ] parent={:?}",
                        parent_val.provenance
                    );

                    let inst = self.module.field_instantiation_at(*idx);
                    
                    let new_prov = match &parent_val.provenance {
                        Provenance::Struct(fields) => {
                            let field_handle = self.module.field_handle_at(inst.handle);
                            let field_idx = field_handle.field as usize;
                            if field_idx < fields.len() {
                                fields[field_idx].clone()
                            } else {
                                Provenance::Field(Box::new(parent_val.provenance.clone()))
                            }
                        },
                        _ => Provenance::Field(Box::new(parent_val.provenance.clone()))
                    };

                    eprintln!(
                        "[MBFG-OUT] result={:?}",
                        new_prov
                    );

                    self.stack.push(AbstractValue {
                        provenance: new_prov,
                    });
                }
            },

            // --- SIGNED INTEGERS & MISSING MATH ---
            Bytecode::LdI8(_) | Bytecode::LdI16(_) | Bytecode::LdI32(_) | 
            Bytecode::LdI64(_) | Bytecode::LdI128(_) | Bytecode::LdI256(_) => {
                self.stack.push(AbstractValue {
                    provenance: Provenance::Bottom,
                });
            },
            Bytecode::CastI8 | Bytecode::CastI16 | Bytecode::CastI32 | 
            Bytecode::CastI64 | Bytecode::CastI128 | Bytecode::CastI256 |
            Bytecode::Negate => {
                self.stack.pop(); // Pop the value being cast/negated
                self.stack.push(AbstractValue::new_bottom());
            },

            // --- ENUMS / VARIANTS (MOVE 2.0) ---
            Bytecode::PackVariant(_) | Bytecode::PackVariantGeneric(_) => {
                // For safety, we treat enums completely as Bottom because resolving their 
                // inner fields dynamically requires Enum-specific module lookup logic.
                // We just need to balance the stack: Pop all fields, push 1 enum.
                // *Note: Because we don't have the field count easily here without Enum 
                // lookup logic, a highly robust implementation would look up the variant def.*
                // For a quick safe-fallback, assuming 1 field variants:
                self.stack.pop(); 
                self.stack.push(AbstractValue::new_bottom());
            },
            Bytecode::UnpackVariant(_) | Bytecode::UnpackVariantGeneric(_) => {
                self.stack.pop(); // Pop the enum
                // Note: Likewise, needs lookup for exact field count. Pushing 1 Bottom to balance.
                self.stack.push(AbstractValue::new_bottom());
            },
            Bytecode::TestVariant(_) | Bytecode::TestVariantGeneric(_) => {
                self.stack.pop(); // Pops the enum reference
                self.stack.push(AbstractValue::new_bottom()); // Pushes boolean
            },
            Bytecode::MutBorrowVariantField(_) | Bytecode::MutBorrowVariantFieldGeneric(_) |
            Bytecode::ImmBorrowVariantField(_) | Bytecode::ImmBorrowVariantFieldGeneric(_) => {
                self.stack.pop(); // Pop Enum ref
                self.stack.push(AbstractValue::new_bottom()); // Push Field ref
            },

            // --- CLOSURES (MOVE 2.0) ---
            Bytecode::PackClosure(_, _) | Bytecode::PackClosureGeneric(_, _) => {
                // A closure packs captured variables. For simplicity, we just push a Bottom closure.
                // (Properly popping the captured variables requires reading the mask, but 
                // if closures aren't heavily used in your workload, this prevents immediate panic).
                self.stack.push(AbstractValue::new_bottom()); 
            },
            Bytecode::CallClosure(_) => {
                // Dynamically popping arguments for a closure is hard without knowing its signature.
                // In an abstract interpreter, hitting a dynamic closure call usually means 
                // we have to downgrade the rest of the block to Bottom to be safe.
                self.stack.push(AbstractValue::new_bottom()); 
            },

            // --- STRAGGLERS ---
            Bytecode::AbortMsg => {
                self.stack.pop(); // Pop the error message
                self.stack.pop(); // Pop the abort code
            },
            Bytecode::Nop => { /* Literally do nothing */ },

            // --- CONTROL FLOW ---
            // These are handled structurally by analyze_with_cfg and are no-ops here:
            // the worklist already accounts for their effect on which block executes next.
            // We still need to pop the condition value for BrTrue/BrFalse to keep the
            // stack balanced within a block's linear execution.
            Bytecode::BrTrue(_) | Bytecode::BrFalse(_) => {
                self.stack.pop(); // consume the boolean condition
            },
            Bytecode::Branch(_) => {
                // Unconditional jump — no stack effect.
            },
            Bytecode::Abort => {
                self.stack.pop(); // consume the error code
            },

            // Arithmetic / Logic / Casts (Pop args, push Bottom — result has no pointer semantics)
            Bytecode::Add | Bytecode::Sub | Bytecode::Mul | Bytecode::Div | Bytecode::Mod | 
            Bytecode::BitAnd | Bytecode::BitOr | Bytecode::Xor | Bytecode::Shl | Bytecode::Shr |
            Bytecode::Or | Bytecode::And | Bytecode::Eq | Bytecode::Neq | 
            Bytecode::Lt | Bytecode::Gt | Bytecode::Le | Bytecode::Ge => {
                self.stack.pop(); self.stack.pop();
                self.stack.push(AbstractValue::new_bottom());
            },
            Bytecode::CastU8 | Bytecode::CastU16 | Bytecode::CastU32 | Bytecode::CastU64 | 
            Bytecode::CastU128 | Bytecode::CastU256 | Bytecode::Not => {
                self.stack.pop();
                self.stack.push(AbstractValue::new_bottom());
            },
            
            Bytecode::Pop => { self.stack.pop(); },

            _ => {} 
        }
    }

    // --- EXECUTION HELPERS ---

    fn execute_call(&mut self, handle_idx: move_binary_format::file_format::FunctionHandleIndex) {
        // A. Resolve Callee
        let func_handle = self.module.function_handle_at(handle_idx);
        let module_handle = self.module.module_handle_at(func_handle.module);
        let module_id = self.module.module_id_for_handle(module_handle);
        let func_name = self.module.identifier_at(func_handle.name).to_string();

        // if func_name.contains("store") || func_name.contains("group") || func_name.contains("borrow") || func_name.contains("pool") || func_name.contains("swap") || func_name.contains("liquidity") {
        //     eprintln!("[CALL-TRACE] {}::{}::{}", 
        //         module_id.address(), module_id.name(), func_name);
        // }
        
        let callee_id = FunctionID {
            address: module_id.address().to_hex_literal(),
            module: module_id.name().to_string(),
            name: func_name.clone(),
        };

        // B. Pop Arguments
        let params_sig_idx = func_handle.parameters;
        let param_types = &self.module.signature_at(params_sig_idx).0;
        let mut args = Vec::new();
        for _ in 0..param_types.len() {
            if let Some(val) = self.stack.pop() {
                args.push(val);
            }
        }
        args.reverse();

        // Functions whose summaries contain unavoidable unknown reads due to
        // the object dispatch system. Their actual global accesses are captured
        // by the caller (coin::transfer) through the concrete FungibleStore
        // addresses already in the write set. Importing their summaries would
        // add spurious ObjectCore@UNKNOWN reads that serialize the entire block.
        let skip_summary = matches!(
            (callee_id.module.as_str(), callee_id.name.as_str()),
            ("fungible_asset", "withdraw_dispatch_function")
            | ("fungible_asset", "unchecked_withdraw")
            | ("dispatchable_fungible_asset", "withdraw")
            | ("dispatchable_fungible_asset", "dispatchable_withdraw")
            | ("primary_fungible_store", "withdraw")

            | ("collateral_balance_sheet", "transfer_to_backstop_liquidator")
            | ("perp_positions", "transfer_balance_to_liquidator")
            | ("accounts_collateral", "transfer_balance_to_liquidator")
            | ("backstop_liquidation", "finalize_backstop_liquidation")
            
            // --- ADDED TO PREVENT SPURIOUS BOTTOM LEAKS FROM MIGRATION BRANCHES ---
            | ("coin", "create_coin_conversion_map")
            | ("coin", "create_pairing")
            | ("coin", "ensure_paired_metadata")
            | ("coin", "migrate_to_fungible_store")
            | ("coin", "migrate_coin_store_to_fungible_store")
            | ("aptos_account", "ensure_primary_fungible_store_exists")
            | ("primary_fungible_store", "ensure_primary_store_exists")
        );

        // C. Apply Summary (Hydrate)
        if !skip_summary {
            if let Some(callee_summary_ref) = self.global_summaries.get(&callee_id) {
                self.apply_callee_summary(&*callee_summary_ref, &args);
            }
        } 

        // C.5. INJECT NATIVE MUTATIONS
        // Track side-effects for natives that don't return values.
        self.handle_native_mutations(&callee_id, &args);

        // D. Handle Return Values
        let return_sig_idx = func_handle.return_;
        let return_types = &self.module.signature_at(return_sig_idx).0;

        for (ret_idx, ret_type) in return_types.iter().enumerate() {
            // STRATEGY 1: Explicit modelling of known natives / framework functions.
            // Returns Some(p) when recognised (p may intentionally be Bottom for scalar
            // returns). Returns None when unknown, falling through to strategies 2 and 3.
            let explicit = self.handle_native_explicit(&callee_id, &args, ret_idx);

            let new_provenance = if let Some(p) = explicit {
                // Recognised — use as-is, no further fallback, no logging.
                p
            } else {
                // STRATEGY 2: Summary return dependency map.
                let mut p = Provenance::Bottom;
                if let Some(callee_summary_ref) = self.global_summaries.get(&callee_id) {
                    if let Some(dep_provenance) = callee_summary_ref.return_dependencies.get(&ret_idx) {
                        p = self.hydrate_provenance(dep_provenance, &args);
                    }
                }

                // STRATEGY 3: Name-based heuristic passthrough — last resort.
                // Only applied when neither strategy 1 nor 2 resolved the provenance.
                // Deliberately narrow: only names that are unambiguously address-like.
                if matches!(p, Provenance::Bottom) {
                    let name = callee_id.name.as_str();
                    let is_passthrough = name.starts_with("get_address")
                                      || name.starts_with("to_address")
                                      || name.starts_with("primary_store")
                                      || name.starts_with("derive_")
                                      || name.ends_with("_address");
                    if is_passthrough {
                        for arg in &args {
                            if !matches!(arg.provenance, Provenance::Bottom) {
                                p = arg.provenance.clone();
                                break;
                            }
                        }
                    }
                }

                 // --- ADD THIS DEBUG BLOCK ---
                if matches!(p, Provenance::Bottom) {
                    eprintln!(
                        "[RETURN-LEAK] Call to {}::{}::{} returned Bottom for ret_idx={}",
                        callee_id.address, callee_id.module, callee_id.name, ret_idx
                    );
                }
                // ----------------------------
                
                p
            };

            self.stack.push(AbstractValue {
                provenance: new_provenance,
            });
        }
    }

    
    /// Explicitly models known native and framework functions whose return provenance cannot
    /// be derived from bytecode alone (either because they are true Rust natives, or because
    /// their Move bytecode performs opaque derivations — hashing, BCS encoding, table
    /// indexing — that our abstract domain cannot symbolically reduce).
    ///
    /// Returns `Some(provenance)` when the function is recognised (Bottom is a valid
    /// intentional result, e.g. for bool-returning queries). Returns `None` when
    /// unrecognised, so the caller falls through to Strategy 2 / Strategy 3 and logs.
    fn handle_native_explicit(&mut self, callee: &FunctionID, args: &Vec<AbstractValue>, ret_idx: usize) -> Option<Provenance> {
        let arg0 = || args.get(0).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);
        let arg1 = || args.get(1).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);

        match (callee.module.as_str(), callee.name.as_str()) {

            // =================================================================
            // 1. TYPE INFO
            // =================================================================
            ("type_info", "type_of") |
            ("type_info", "account_address") |
            ("type_info", "struct_name") |
            ("type_info", "module_name") |
            ("type_info", "type_name") => Some(Provenance::TypeArg(0)),

            // =================================================================
            // 2. TABLES & MAPS
            // =================================================================
            // READS / BORROWS: Return the map's provenance so future writes track back to it.
            ("table", "borrow") | ("table", "borrow_mut") |
            ("table", "borrow_box") | ("table", "borrow_box_mut") |
            ("smart_table", "borrow") | ("smart_table", "borrow_mut") |
            ("smart_table", "borrow_with_default") | ("smart_table", "borrow_mut_with_default") |
            ("table_with_length", "borrow") | ("table_with_length", "borrow_mut") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // RETURNS FROM MUTATIONS: Only functions that return a value go here.
            ("table", "remove") | ("table", "remove_box") |
            ("smart_table", "remove") | 
            ("table_with_length", "remove")  => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // AMM / DEX / ORDERBOOK SPECIFIC MUTABLE GETTERS
            // These functions extract a nested &mut field from a parent struct.
            // Returning Field(arg0) ensures writes to the child trace back to the parent.
            // =================================================================
            ("market_types", "get_order_book_mut") |
            ("market_types", "get_order_book") |
            ("bulk_order_types", "get_order_request_mut") |
            ("bulk_order_types", "get_order_request") |
            ("bulk_order_types", "get_prices_and_sizes_mut") |
            ("collateral_balance_sheet", "get_secondary_balance_mut") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // DEX MATCHING ENGINE GETTERS
            // =================================================================
            ("order_book", "get_single_match_for_taker") |
            ("bulk_order_book", "get_single_match_for_taker") |
            ("single_order_book", "get_single_match_for_taker") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // 3. OPTIONS
            // =================================================================
            
            // CONSTRUCTORS, EXTRACTORS, & BORROWS
            // Functions that either pack T into Option<T>, or extract T out of it.
            // Returning arg0 strictly pipes the element's provenance forward.
            ("option", "some") |
            ("option", "spec_some") |
            ("option", "from_vec") |
            ("option", "borrow") |
            ("option", "spec_borrow") |
            ("option", "borrow_mut") |
            ("option", "borrow_with_default") |
            ("option", "get_with_default") |
            ("option", "extract") |
            ("option", "swap") |
            ("option", "swap_or_fill") |
            ("option", "destroy_with_default") |
            ("option", "destroy_some") |
            ("option", "to_vec") |
            ("option", "map") |
            ("option", "map_ref") |
            ("option", "filter") => Some(arg0()),

            // FOLD INLINE
            // fold(self: Option<Element>, init: Accumulator, ...)
            // Returns the accumulator, which gets its initial provenance from `init` (Arg 1).
            ("option", "fold") => Some(arg1()),

            // EMPTY OPTIONS, SCALARS, BOOLEANS, & UNIT RETURNS
            ("option", "none") |
            ("option", "spec_none") |
            ("option", "is_none") |
            ("option", "spec_is_none") |
            ("option", "is_some") |
            ("option", "spec_is_some") |
            ("option", "contains") |
            ("option", "spec_contains") |
            ("option", "any") |
            ("option", "fill") |
            ("option", "destroy_none") |
            ("option", "for_each") |
            ("option", "for_each_ref") |
            ("option", "for_each_mut") |
            ("option", "destroy") => Some(Provenance::Bottom),

            // =================================================================
            // 3.5. RESULTS (from result.rs)
            //    Containers for values. Provenance flows from the wrapped item.
            // =================================================================
            ("result", "ok") | ("result", "err") |
            ("result", "unwrap") | ("result", "unwrap_err") |
            // result.rs specific internal naming
            ("result", "ok_result") | ("result", "err_result") |
            ("result", "unwrap_result") | ("result", "unwrap_err_result") => Some(arg0()),

            ("result", "is_ok") | ("result", "is_err") |
            ("result", "is_ok_result_ref") => Some(Provenance::Bottom),

            // =================================================================
            // 4. OBJECT — address derivations, ref helpers, and queries
            // =================================================================
            
            // ADDRESS, REF, & OBJECT RETURNING
            // Functions that derive, extract, or wrap an address/object/ref.
            // Returning arg0 passes the provenance of the source correctly downstream.
            ("object", "address_to_object") |
            ("object", "create_object_address") |
            ("object", "create_user_derived_object_address_impl") |
            ("object", "create_user_derived_object_address") |
            ("object", "create_guid_object_address") |
            ("object", "object_address") |
            ("object", "convert") |
            ("object", "create_named_object") |
            ("object", "create_user_derived_object") |
            ("object", "create_object") |
            ("object", "create_sticky_object") |
            ("object", "create_sticky_object_at_address") |
            ("object", "create_object_from_account") |
            ("object", "create_object_from_object") |
            ("object", "create_object_from_guid") |
            ("object", "create_object_internal") |
            ("object", "generate_delete_ref") |
            ("object", "generate_extend_ref") |
            ("object", "generate_transfer_ref") |
            ("object", "generate_derive_ref") |
            ("object", "generate_signer") |
            ("object", "address_from_constructor_ref") |
            ("object", "object_from_constructor_ref") |
            ("object", "address_from_delete_ref") |
            ("object", "object_from_delete_ref") |
            ("object", "generate_signer_for_extending") |
            ("object", "address_from_extend_ref") |
            ("object", "generate_linear_transfer_ref") |
            ("object", "owner") => Some(arg0()),

            // SCALAR & BOOLEAN QUERIES (Return Bottom)
            ("object", "is_untransferable") |
            ("object", "is_burnt") |
            ("object", "is_object") |
            ("object", "object_exists") |
            ("object", "exists_at") |
            ("object", "can_generate_delete_ref") |
            ("object", "ungated_transfer_allowed") |
            ("object", "is_owner") |
            ("object", "owns") |
            ("object", "root_owner") => Some(Provenance::Bottom),

            // GUID & EVENT HANDLES (Return Bottom)
            ("object", "create_guid") |
            ("object", "new_event_handle") => Some(Provenance::Bottom),

            // UNIT RETURNING / MUTATIONS (Return Bottom, side-effects handled separately)
            ("object", "delete") |
            ("object", "disable_ungated_transfer") |
            ("object", "set_untransferable") |
            ("object", "enable_ungated_transfer") |
            ("object", "transfer_with_ref") |
            ("object", "transfer_call") |
            ("object", "transfer") |
            ("object", "transfer_raw") |
            ("object", "transfer_to_object") |
            ("object", "burn") |
            ("object", "unburn") |
            ("object", "grant_permission") |
            ("object", "grant_permission_with_transfer_ref") |
            ("object", "burn_object_with_transfer") => Some(Provenance::Bottom),

            // =================================================================
            // 5. SIGNER / ADDRESS UTILS — identity transformations on addresses.
            // =================================================================
            ("signer", "address_of") | ("signer", "borrow_address") |
            ("util", "from_bytes") | ("from_bcs", "to_address") |
            ("bcs", "to_bytes") |
            ("hash", "sha3_256") | ("hash", "sha2_256") | 
            ("aptos_hash", "keccak256") | ("aptos_hash", "blake2b_256") | // ADDED
            ("aptos_hash", "ripemd160") | ("aptos_hash", "sip_hash_from_value") | // ADDED
            ("create_signer", "create_signer") => Some(arg0()),

            // =================================================================
            // 6. VECTOR & SMART VECTOR — Container Provenance Pipeline
            // =================================================================
            ("vector", "borrow") | ("vector", "borrow_mut") |
            ("smart_vector", "borrow") | ("smart_vector", "borrow_mut") => Some(arg0()),

            // Popped values inherit the proxy provenance
            ("vector", "pop_back") | ("vector", "remove") | ("vector", "swap_remove") |
            ("smart_vector", "pop_back") | ("smart_vector", "remove") | ("smart_vector", "swap_remove") => Some(arg0()),

            // Pure queries
            ("vector", "is_empty") | ("vector", "length") | ("vector", "contains") | ("vector", "index_of") |
            ("smart_vector", "is_empty") | ("smart_vector", "length") | ("smart_vector", "contains") | 
            ("smart_vector", "index_of") | ("smart_vector", "new") => Some(Provenance::Bottom),

            // PURE FUNCTIONS (No direct global mutation of arg0)
            ("bulk_order_types", "get_active_price") |
            ("bulk_order_types", "get_active_size") |
            ("bulk_order_types", "get_order_id") |
            ("bulk_order_types", "get_unique_priority_idx") |
            ("single_order_types", "get_price") |
            ("single_order_types", "get_remaining_size") |
            ("single_order_types", "get_client_order_id") |
            ("single_order_types", "get_unique_priority_idx") => {
                Some(Provenance::Bottom)
            },

            // =================================================================
            // MAP ITERATORS & POINTER GETTERS
            // Passing provenance through iterators so iter_modify knows what to write to
            // =================================================================
            
            ("single_order_types", "get_order_request") |
            ("single_order_types", "get_order_from_state") => {
                // The returned iterator/reference inherits the Map's provenance!
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // ORDERED MAP
            // =================================================================

            // EXTRACTORS & BORROWS
            // Functions that return &V, &mut V, V, Options, or Vectors of keys/values.
            // Return Field(arg0) because they extract a value/key from the map (or IteratorPtr).
            ("ordered_map", "borrow") |
            ("ordered_map", "borrow_mut") |
            ("ordered_map", "borrow_front") |
            ("ordered_map", "borrow_back") |
            ("ordered_map", "get") |
            ("ordered_map", "get_and_map") |
            ("ordered_map", "upsert") |
            ("ordered_map", "remove") |
            ("ordered_map", "remove_or_none") |
            ("ordered_map", "pop_front") |
            ("ordered_map", "pop_back") |
            ("ordered_map", "prev_key") |
            ("ordered_map", "next_key") |
            ("ordered_map", "keys") |
            ("ordered_map", "values") |
            ("ordered_map", "to_vec_pair") |
            ("ordered_map", "append_impl") |
            ("ordered_map", "iter_borrow") |
            ("ordered_map", "iter_borrow_mut") |
            ("ordered_map", "iter_borrow_key") |
            ("ordered_map", "iter_remove") |
            ("ordered_map", "iter_replace") => Some(Provenance::Field(Box::new(arg0()))),

            // ITERATORS & MAP WRAPPERS
            // These return IteratorPtrs or modified Maps. Return arg0 to pass provenance.
            ("ordered_map", "internal_lower_bound") |
            ("ordered_map", "internal_find") |
            ("ordered_map", "internal_new_begin_iter") |
            ("ordered_map", "internal_new_end_iter") |
            ("ordered_map", "iter_next") |
            ("ordered_map", "iter_prev") |
            ("ordered_map", "trim") => Some(arg0()),

            // CONSTRUCTORS, SCALARS, & BOOLEANS
            // Map creations, length checks, bool checks, and tests return Bottom.
            ("ordered_map", "new") |
            ("ordered_map", "new_from") |
            ("ordered_map", "length") |
            ("ordered_map", "is_empty") |
            ("ordered_map", "contains") |
            ("ordered_map", "iter_is_begin") |
            ("ordered_map", "iter_is_begin_from_non_empty") |
            ("ordered_map", "iter_is_end") |
            ("ordered_map", "destroy_empty") |
            ("ordered_map", "new_iter") |
            ("ordered_map", "binary_search") |
            ("ordered_map", "print_map") |
            ("ordered_map", "validate_ordered") |
            ("ordered_map", "validate_iteration") |
            ("ordered_map", "validate_map") |
            ("ordered_map", "large_dataset") |
            ("ordered_map", "large_dataset_shuffled") => Some(Provenance::Bottom),

            // =================================================================
            // BIG ORDERED MAP
            // =================================================================

            // EXTRACTORS & BORROWS
            // Functions that return &V, &mut V, V, Options, or Vectors of keys/values.
            // We return Field(arg0) so mutations applied to the returned struct correctly 
            // trace back as modifying a sub-field of the Map itself.
            ("big_ordered_map", "borrow") |
            ("big_ordered_map", "borrow_mut") |
            ("big_ordered_map", "borrow_front") |
            ("big_ordered_map", "borrow_back") |
            ("big_ordered_map", "get") |
            ("big_ordered_map", "get_and_map") |
            ("big_ordered_map", "upsert") |
            ("big_ordered_map", "remove") |
            ("big_ordered_map", "remove_or_none") |
            ("big_ordered_map", "pop_front") |
            ("big_ordered_map", "pop_back") |
            ("big_ordered_map", "iter_borrow") |
            ("big_ordered_map", "iter_borrow_mut") |
            ("big_ordered_map", "iter_borrow_key") |
            ("big_ordered_map", "internal_leaf_borrow_value") |
            ("big_ordered_map", "internal_leaf_iter_borrow_entries_and_next_leaf_index") |
            ("big_ordered_map", "iter_remove") |
            ("big_ordered_map", "keys") |
            ("big_ordered_map", "to_ordered_map") => Some(Provenance::Field(Box::new(arg0()))),

            // ITERATORS & INTERNAL STRUCTS
            // These return IteratorPtrs, Child nodes, or Nodes that wrap the map.
            // Returning arg0 strictly pipes the map's provenance forward.
            ("big_ordered_map", "internal_lower_bound") |
            ("big_ordered_map", "internal_find") |
            ("big_ordered_map", "internal_find_with_path") |
            ("big_ordered_map", "iter_with_path_get_iter") |
            ("big_ordered_map", "internal_new_begin_iter") |
            ("big_ordered_map", "internal_new_end_iter") |
            ("big_ordered_map", "iter_next") |
            ("big_ordered_map", "iter_prev") |
            ("big_ordered_map", "internal_leaf_new_begin_iter") |
            ("big_ordered_map", "add_at") |
            ("big_ordered_map", "add_or_upsert_impl") |
            ("big_ordered_map", "replace_root") |
            ("big_ordered_map", "remove_at") |
            ("big_ordered_map", "new_iter") |
            ("big_ordered_map", "new_node") |
            ("big_ordered_map", "new_node_with_children") |
            ("big_ordered_map", "new_inner_child") |
            ("big_ordered_map", "new_leaf_child") => Some(arg0()),

            // CONSTRUCTORS, SCALARS, & BOOLEANS
            // Map creations, queries for u64 length, and bool checks return Bottom.
            ("big_ordered_map", "new") |
            ("big_ordered_map", "new_with_reusable") |
            ("big_ordered_map", "new_with_type_size_hints") |
            ("big_ordered_map", "new_with_config") |
            ("big_ordered_map", "new_from") |
            ("big_ordered_map", "is_empty") |
            ("big_ordered_map", "compute_length") |
            ("big_ordered_map", "contains") |
            ("big_ordered_map", "front_key") |
            ("big_ordered_map", "back_key") |
            ("big_ordered_map", "prev_key") |
            ("big_ordered_map", "next_key") |
            ("big_ordered_map", "iter_is_begin") |
            ("big_ordered_map", "iter_is_end") |
            ("big_ordered_map", "internal_leaf_iter_is_end") |
            ("big_ordered_map", "find_leaf") |
            ("big_ordered_map", "find_leaf_path") |
            ("big_ordered_map", "get_max_degree") |
            ("big_ordered_map", "vector_range") |
            ("big_ordered_map", "vector_bytes_range") => Some(Provenance::Bottom),

            // =================================================================
            // ORDER TYPES & ADDRESS GETTERS
            // The address extracted belongs to the struct passed as arg0
            // =================================================================
            ("single_order_types", "get_account") |
            ("order_match_types", "get_account_from_match_details") |
            ("bulk_order_types", "get_account") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // 8. STORAGE SLOTS ALLOCATOR
            // =================================================================
            
            // EXTRACTORS & BORROWS
            // Functions returning T, &T, &mut T, or Link<T> from the allocator (Arg 0)
            // They extract a value, so it correctly becomes a Field of the parent container.
            ("storage_slots_allocator", "borrow") |
            ("storage_slots_allocator", "borrow_mut") |
            ("storage_slots_allocator", "remove") |
            ("storage_slots_allocator", "remove_link") => {
                Some(Provenance::Field(Box::new(arg0())))
            },
            
            // remove_and_reserve returns (ReservedSlot, T). 
            // ret_idx 0 is a u64 wrapper (Bottom). ret_idx 1 is the extracted item T.
            ("storage_slots_allocator", "remove_and_reserve") => {
                if ret_idx == 1 { 
                    Some(Provenance::Field(Box::new(arg0()))) 
                } else { 
                    Some(Provenance::Bottom) 
                }
            },
            
            // CONSTRUCTORS, SCALARS, BOOLEANS, SLOTS, & UNIT RETURNS
            // All slot structures (StoredSlot, ReservedSlot) are index wrappers without pointer semantics.
            ("storage_slots_allocator", "new") |
            ("storage_slots_allocator", "get_num_spare_slot_count") |
            ("storage_slots_allocator", "add") |
            ("storage_slots_allocator", "destroy_empty") |
            ("storage_slots_allocator", "reserve_slot") |
            ("storage_slots_allocator", "reserved_to_index") |
            ("storage_slots_allocator", "stored_to_index") |
            ("storage_slots_allocator", "is_null_index") |
            ("storage_slots_allocator", "is_special_unused_index") |
            ("storage_slots_allocator", "maybe_pop_from_reuse_queue") |
            ("storage_slots_allocator", "next_slot_index") |
            ("storage_slots_allocator", "allocate_spare_slots") |
            ("storage_slots_allocator", "fill_reserved_slot") |
            ("storage_slots_allocator", "free_reserved_slot") |
            ("storage_slots_allocator", "maybe_push_to_reuse_queue") |
            ("storage_slots_allocator", "add_link") => Some(Provenance::Bottom),

            // =================================================================
            // 9. AGGREGATORS — Opaque counters, factories, and snapshots
            // =================================================================
            // All aggregator functions return scalar values, internal handles 
            // (Aggregator, OptionalAggregator), Snapshots, or booleans. 
            // There are no address pointers to track here.
            ("aggregator", _) |
            ("aggregator_v2", _) |
            ("aggregator_factory", _) |
            ("optional_aggregator", _) => Some(Provenance::Bottom),

            // =================================================================
            // 10. CMP — Ordering scalar, never a pointer.
            // =================================================================
            ("cmp", "compare") |
            ("cmp", "is_lt") | ("cmp", "is_gt") | ("cmp", "is_eq") => Some(Provenance::Bottom),

            // =================================================================
            // 11. ERROR CONSTRUCTORS — return u64 error codes, not pointers.
            // =================================================================
            ("error", "invalid_state") | ("error", "invalid_argument") |
            ("error", "not_found") | ("error", "already_exists") |
            ("error", "permission_denied") | ("error", "out_of_range") |
            ("error", "internal") => Some(Provenance::Bottom),

            // =================================================================
            // 12. FEATURES — bool flag checks and getters.
            // =================================================================
            // Every single function in the `features` module returns either a `bool`,
            // a `u64` feature flag ID, or `()` (unit). There are no pointers, addresses, 
            // or objects returned by this module. Thus, we can safely wildcard the 
            // entire module to return Bottom.
            ("features", _) => Some(Provenance::Bottom),

            // =================================================================
            // 13. PERMISSIONED SIGNER
            // =================================================================
            
            // SIGNER & ADDRESS DERIVATIONS / CAPABILITY WRAPPERS
            // These functions create or extract addresses/signers from master or handles.
            // By returning arg0, we retain the provenance of the master/handle driving the logic.
            ("permissioned_signer", "create_permissioned_handle") |
            ("permissioned_signer", "signer_from_permissioned_handle") |
            ("permissioned_signer", "create_storable_permissioned_handle") |
            ("permissioned_signer", "signer_from_storable_permissioned_handle") |
            ("permissioned_signer", "permissions_storage_address") |
            ("permissioned_signer", "address_of") |
            ("permissioned_signer", "borrow_address") |
            ("permissioned_signer", "permission_address") |
            ("permissioned_signer", "signer_from_permissioned_handle_impl") => Some(arg0()),

            // SCALAR QUERIES & VALUE TYPES
            ("permissioned_signer", "is_permissioned_signer") |
            ("permissioned_signer", "is_permissioned_signer_impl") |
            ("permissioned_signer", "check_permission_exists") |
            ("permissioned_signer", "check_permission_capacity_above") |
            ("permissioned_signer", "check_permission_consume") |
            ("permissioned_signer", "capacity") |
            ("permissioned_signer", "is_above") => Some(Provenance::Bottom),

            // UNIT RETURNING (Mutations captured below or via body walk)
            ("permissioned_signer", "destroy_permissioned_handle") |
            ("permissioned_signer", "destroy_storable_permissioned_handle") |
            ("permissioned_signer", "grant_revoke_permission") |
            ("permissioned_signer", "revoke_permission_storage_address") |
            ("permissioned_signer", "revoke_all_handles") |
            ("permissioned_signer", "initialize_permission_address") |
            ("permissioned_signer", "destroy_permissions_storage_address") |
            ("permissioned_signer", "assert_master_signer") |
            ("permissioned_signer", "consume_capacity") |
            ("permissioned_signer", "increase_capacity") |
            ("permissioned_signer", "merge") |
            ("permissioned_signer", "authorize_increase") |
            ("permissioned_signer", "authorize_unlimited") |
            ("permissioned_signer", "grant_unlimited_with_permissioned_signer") |
            ("permissioned_signer", "increase_limit") |
            ("permissioned_signer", "revoke_permission") |
            ("permissioned_signer", "signer_address_roundtrip") |
            ("permissioned_signer", "signer_serialization") => Some(Provenance::Bottom),

            // =================================================================
            // 14. MISC OPAQUE CONSTRUCTORS / SCALAR RETURNS / NATIVES
            // =================================================================
            ("table", "new_table_handle") |
            ("table", "contains") |
            ("table", "contains_box") |
            ("table_with_length", "contains") |
            ("smart_table", "contains") |
            ("bcs", "serialized_size") |
            ("bcs", "constant_serialized_size") | // ADDED (from bcs.rs)
            ("from_bcs", "from_bytes") |
            ("string", "internal_check_utf8") |
            ("string", "internal_is_char_boundary") | // ADDED (from string.rs)
            ("string", "internal_sub_string") |       // ADDED (from string.rs)
            ("string", "internal_index_of") |         // ADDED (from string.rs)
            ("mem", "swap") |                         // ADDED (from mem.rs - unit return)
            ("vector", "move_range") |                // ADDED (from vector.rs - unit return)
            ("unit_test", "create_signers_for_testing") | // ADDED (from unit_test.rs)
            ("reflect", "native_resolve") => Some(Provenance::Bottom), // ADDED (from reflect.rs)

            // =================================================================
            // 15. FUNGIBLE ASSET
            //
            // borrow_store_resource(&store) → &FungibleStore at store's address.
            // IMPORTANT: now uncommented. This is the critical path that allows
            // WriteRef downstream to correctly record FungibleStore writes.
            // arg0 is Object<T> (store handle); the store address is its inner field,
            // which the interpreter already tracks as arg0's provenance.
            // =================================================================
            ("fungible_asset", "borrow_store_resource") => {
                Some(Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()),
                })
            },

            // The concurrent balance accessor — same pattern as borrow_store_resource.
            // Callers write through this ref to update ConcurrentFungibleBalance.
            ("fungible_asset", "borrow_concurrent_balance_resource") |
            ("fungible_asset", "borrow_concurrent_balance_resource_mut") => {
                Some(Provenance::Global {
                    resource_type: "0x1::fungible_asset::ConcurrentFungibleBalance".to_string(),
                    address_source: Box::new(arg0()),
                })
            },

            // =================================================================
            // FUNGIBLE ASSET CONSTRUCTORS & RETURNS (NO SIDE EFFECTS HERE)
            // =================================================================
            ("fungible_asset", "create_store") |
            ("fungible_asset", "add_fungibility") => Some(arg0()),

            ("fungible_asset", "withdraw") |
            ("fungible_asset", "unchecked_withdraw") |
            ("fungible_asset", "withdraw_with_ref") |
            ("fungible_asset", "withdraw_sanity_check") |
            ("fungible_asset", "withdraw_permission_check") |
            ("fungible_asset", "withdraw_permission_check_by_address") => Some(Provenance::Bottom),

            // =================================================================
            // FUNGIBLE ASSET (PURE / VALUE OP / CAPABILITIES)
            // =================================================================
            ("fungible_asset", "zero") |
            ("fungible_asset", "extract") |
            ("fungible_asset", "merge") |
            ("fungible_asset", "generate_mint_ref") |
            ("fungible_asset", "generate_burn_ref") |
            ("fungible_asset", "generate_burn_copy_ref") |
            ("fungible_asset", "generate_transfer_ref") |
            ("fungible_asset", "generate_raw_balance_ref") |
            ("fungible_asset", "generate_raw_supply_ref") |
            ("fungible_asset", "generate_mutate_metadata_ref") => {
                Some(Provenance::Bottom)
            },

            // =================================================================
            // FUNGIBLE ASSET (READS & QUERIES)
            // =================================================================
            ("fungible_asset", "withdraw_dispatch_function") |
            ("fungible_asset", "deposit_dispatch_function") |
            ("fungible_asset", "derived_balance_dispatch_function") |
            ("fungible_asset", "derived_supply_dispatch_function") |
            ("fungible_asset", "balance") |
            ("fungible_asset", "balance_impl") |
            ("fungible_asset", "balance_with_ref") |
            ("fungible_asset", "supply_with_ref") |
            ("fungible_asset", "is_frozen") |
            ("fungible_asset", "store_exists") |
            ("fungible_asset", "is_untransferable") |
            ("fungible_asset", "is_store_dispatchable") |
            ("fungible_asset", "has_withdraw_dispatch_function") |
            ("fungible_asset", "has_deposit_dispatch_function") |
            ("fungible_asset", "has_balance_dispatch_function") |
            ("fungible_asset", "has_supply_dispatch_function") |
            ("fungible_asset", "is_address_balance_at_least") |
            ("fungible_asset", "is_balance_at_least") |
            ("fungible_asset", "amount") |
            ("fungible_asset", "supply") |
            ("fungible_asset", "supply_impl") |
            ("fungible_asset", "maximum") |
            ("fungible_asset", "name") |
            ("fungible_asset", "symbol") |
            ("fungible_asset", "decimals") |
            ("fungible_asset", "icon_uri") |
            ("fungible_asset", "project_uri") |
            ("fungible_asset", "metadata") |
            ("fungible_asset", "store_metadata") |
            ("fungible_asset", "metadata_from_asset") |
            ("fungible_asset", "asset_metadata") |
            ("fungible_asset", "mint_ref_metadata") |
            ("fungible_asset", "transfer_ref_metadata") |
            ("fungible_asset", "burn_ref_metadata") |
            ("fungible_asset", "object_from_metadata_ref") => {
                Some(Provenance::Bottom)
            },

            // =================================================================
            // 16. PRIMARY FUNGIBLE STORE
            // =================================================================

            // OBJECT / ADDRESS RETURNING
            // Functions returning Object<FungibleStore> or a derived address.
            // arg0 = owner address (or &signer), which serves as an excellent 
            // static approximation for the store's provenance.
            ("primary_fungible_store", "primary_store") |
            ("primary_fungible_store", "primary_store_inlined") |
            ("primary_fungible_store", "primary_store_address") |
            ("primary_fungible_store", "primary_store_address_inlined") |
            ("primary_fungible_store", "ensure_primary_store_exists") |
            ("primary_fungible_store", "create_primary_store") |
            ("primary_fungible_store", "init_test_metadata_with_primary_store_enabled") => Some(arg0()),

            // SCALAR QUERIES & VALUE TYPES
            ("primary_fungible_store", "primary_store_exists") |
            ("primary_fungible_store", "primary_store_exists_inlined") |
            ("primary_fungible_store", "balance") |
            ("primary_fungible_store", "is_balance_at_least") |
            ("primary_fungible_store", "is_frozen") |
            ("primary_fungible_store", "withdraw") |
            ("primary_fungible_store", "withdraw_with_ref") => Some(Provenance::Bottom),

            // UNIT RETURNING (Mutations captured below in handle_native_mutations)
            ("primary_fungible_store", "deposit") |
            ("primary_fungible_store", "deposit_with_signer") |
            ("primary_fungible_store", "deposit_with_ref") |
            ("primary_fungible_store", "transfer") |
            ("primary_fungible_store", "transfer_assert_minimum_deposit") |
            ("primary_fungible_store", "transfer_with_ref") |
            ("primary_fungible_store", "mint") |
            ("primary_fungible_store", "burn") |
            ("primary_fungible_store", "set_frozen_flag") |
            ("primary_fungible_store", "grant_permission") |
            ("primary_fungible_store", "grant_apt_permission") |
            ("primary_fungible_store", "create_primary_store_enabled_fungible_asset") |
            ("primary_fungible_store", "may_be_unburn") => Some(Provenance::Bottom),

            // =================================================================
            // 17. DISPATCHABLE FUNGIBLE ASSET
            // =================================================================
            
            // REFERENCE GETTERS
            // borrow_transfer_ref returns the internal wrapper, passing provenance forward.
            ("dispatchable_fungible_asset", "borrow_transfer_ref") => Some(arg0()),

            // VALUE TYPES, SCALARS, AND UNIT RETURNS
            ("dispatchable_fungible_asset", "withdraw") |
            ("dispatchable_fungible_asset", "dispatchable_withdraw") |
            ("dispatchable_fungible_asset", "deposit") |
            ("dispatchable_fungible_asset", "dispatchable_deposit") |
            ("dispatchable_fungible_asset", "transfer") |
            ("dispatchable_fungible_asset", "transfer_assert_minimum_deposit") |
            ("dispatchable_fungible_asset", "derived_balance") |
            ("dispatchable_fungible_asset", "dispatchable_derived_balance") |
            ("dispatchable_fungible_asset", "is_derived_balance_at_least") |
            ("dispatchable_fungible_asset", "derived_supply") |
            ("dispatchable_fungible_asset", "dispatchable_derived_supply") |
            ("dispatchable_fungible_asset", "register_dispatch_functions") |
            ("dispatchable_fungible_asset", "register_derive_supply_dispatch_function") => Some(Provenance::Bottom),

            // =================================================================
            // 18. COIN
            // =================================================================
            
            // VALUE RETURNING
            // Coin<T> objects don't carry dynamic pointer addresses (unlike Objects). 
            ("coin", "withdraw") |
            ("coin", "mint") |
            ("coin", "mint_internal") |
            ("coin", "zero") |
            ("coin", "extract") |
            ("coin", "extract_all") |
            ("coin", "coin_to_fungible_asset") |
            ("coin", "fungible_asset_to_coin") => Some(Provenance::Bottom),

            // CAPABILITY RETURNING / METADATA
            ("coin", "paired_metadata") |
            ("coin", "ensure_paired_metadata") |
            ("coin", "paired_coin") |
            ("coin", "get_paired_mint_ref") |
            ("coin", "get_paired_transfer_ref") |
            ("coin", "get_paired_burn_copy_ref") |
            ("coin", "get_paired_burn_ref") |
            ("coin", "convert_and_take_paired_burn_ref") |
            ("coin", "borrow_paired_burn_ref") |
            ("coin", "initialize") |
            ("coin", "initialize_with_parallelizable_supply") |
            ("coin", "initialize_internal") => Some(Provenance::Bottom),

            // READS & SCALAR QUERIES
            ("coin", "balance") |
            ("coin", "coin_balance") |
            ("coin", "is_balance_at_least") |
            ("coin", "is_coin_initialized") |
            ("coin", "is_coin_store_frozen") |
            ("coin", "is_account_registered") |
            ("coin", "supply") |
            ("coin", "coin_supply") |
            ("coin", "name") |
            ("coin", "symbol") |
            ("coin", "decimals") |
            ("coin", "paired_mint_ref_exists") |
            ("coin", "paired_transfer_ref_exists") |
            ("coin", "paired_burn_ref_exists") |
            ("coin", "is_apt") |
            ("coin", "value") |
            ("coin", "coin_address") => Some(Provenance::Bottom),

            // UNIT RETURNING (Writes are handled by body walk or handle_native_mutations)
            ("coin", "burn") |
            ("coin", "burn_internal") |
            ("coin", "burn_from") |
            ("coin", "burn_from_for_gas") |
            ("coin", "deposit") |
            ("coin", "deposit_with_signer") |
            ("coin", "deposit_for_gas_fee") |
            ("coin", "destroy_zero") |
            ("coin", "merge") |
            ("coin", "freeze_coin_store") |
            ("coin", "unfreeze_coin_store") |
            ("coin", "upgrade_supply") |
            ("coin", "register") |
            ("coin", "transfer") |
            ("coin", "destroy_freeze_cap") |
            ("coin", "destroy_mint_cap") |
            ("coin", "destroy_burn_cap") |
            ("coin", "return_paired_mint_ref") |
            ("coin", "return_paired_transfer_ref") |
            ("coin", "return_paired_burn_ref") |
            ("coin", "allow_supply_upgrades") |
            ("coin", "migrate_to_fungible_store") |
            ("coin", "migrate_coin_store_to_fungible_store") |
            ("coin", "create_coin_conversion_map") |
            ("coin", "create_pairing") => Some(Provenance::Bottom),

            // =================================================================
            // 19. ACCOUNT & 21. APTOS_ACCOUNT
            // =================================================================

            // SIGNER & ADDRESS DERIVATIONS
            // Functions returning signers or derived addresses where arg0 is the primary source.
            ("account", "create_signer_for_test") |
            ("account", "create_account_for_test") |
            ("account", "create_account") |
            ("account", "create_account_unchecked") |
            ("account", "create_resource_address") |
            ("account", "create_resource_account") |
            ("account", "create_framework_reserved_account") |
            ("account", "create_test_signer_cap") |
            ("aptos_account", "ensure_primary_fungible_store_exists") |
            ("aptos_account", "primary_fungible_store_address") => Some(arg0()),

            // create_authorized_signer(account: &signer, offerer_address: address): signer
            // The derived signer is for the offerer_address, which is Arg 1!
            ("account", "create_authorized_signer") => Some(arg1()),

            // QUERIES, EVENT HANDLERS, & UNIT RETURNS
            ("account", "exists_at") |
            ("account", "resource_exists_at") |
            ("account", "get_guid_next_creation_num") |
            ("account", "get_sequence_number") |
            ("account", "originating_address") |
            ("account", "get_authentication_key") |
            ("account", "is_rotation_capability_offered") |
            ("account", "get_rotation_capability_offer_for") |
            ("account", "is_signer_capability_offered") |
            ("account", "get_signer_capability_offer_for") |
            ("account", "get_signer_capability_address") |
            ("account", "get_signer_capability_offer_proof_challenge_v2") |
            ("account", "create_guid") |
            ("account", "new_event_handle") |
            ("account", "create_account_if_does_not_exist") |
            ("account", "grant_key_rotation_permission") |
            ("account", "grant_key_offering_permission") |
            ("account", "increment_sequence_number") |
            ("account", "rotate_authentication_key_internal") |
            ("account", "rotate_authentication_key_call") |
            ("account", "rotate_authentication_key") |
            ("account", "rotate_authentication_key_from_public_key") |
            ("account", "upsert_ed25519_backup_key_on_keyless_account") |
            ("account", "rotate_authentication_key_with_rotation_capability") |
            ("account", "offer_rotation_capability") |
            ("account", "revoke_rotation_capability") |
            ("account", "revoke_any_rotation_capability") |
            ("account", "offer_signer_capability") |
            ("account", "revoke_signer_capability") |
            ("account", "revoke_any_signer_capability") |
            ("account", "set_originating_address") |
            ("account", "verify_signed_message") |
            ("account", "set_sequence_number") |
            ("account", "set_signer_capability_offer") |
            ("account", "set_rotation_capability_offer") |
            ("account", "register_coin") |
            ("aptos_account", "can_receive_direct_coin_transfers") |
            ("aptos_account", "is_fungible_balance_at_least") |
            ("aptos_account", "assert_account_exists") |
            ("aptos_account", "assert_account_is_registered_for_apt") |
            ("aptos_account", "create_account") |
            ("aptos_account", "batch_transfer") |
            ("aptos_account", "transfer") |
            ("aptos_account", "batch_transfer_coins") |
            ("aptos_account", "transfer_coins") |
            ("aptos_account", "deposit_coins") |
            ("aptos_account", "batch_transfer_fungible_assets") |
            ("aptos_account", "transfer_fungible_assets") |
            ("aptos_account", "deposit_fungible_assets") |
            ("aptos_account", "set_allow_direct_coin_transfers") |
            ("aptos_account", "register_apt") |
            ("aptos_account", "fungible_transfer_only") |
            ("aptos_account", "burn_from_fungible_store_for_gas") => Some(Provenance::Bottom),

            // =================================================================
            // TOKEN V1 & V2 (aptos_token::token & aptos_token_objects::token)
            // =================================================================
            
            // BORROWS & EXTRACTORS (Token V2 & Value types)
            ("token", "borrow") |
            ("token", "borrow_mut") => Some(Provenance::Field(Box::new(arg0()))),

            // OBJECT & REF CONSTRUCTORS (Token V2)
            // Functions that derive, extract, or wrap an address/object/ref.
            ("token", "create") |
            ("token", "create_token") |
            ("token", "create_token_as_collection_owner") |
            ("token", "create_numbered_token_object") |
            ("token", "create_numbered_token") |
            ("token", "create_numbered_token_as_collection_owner") |
            ("token", "create_named_token_object") |
            ("token", "create_named_token") |
            ("token", "create_named_token_as_collection_owner") |
            ("token", "create_named_token_from_seed") |
            ("token", "create_named_token_from_seed_as_collection_owner") |
            ("token", "create_from_account") |
            ("token", "create_token_address") |
            ("token", "create_token_address_with_seed") |
            ("token", "generate_mutator_ref") |
            ("token", "generate_burn_ref") |
            ("token", "address_from_burn_ref") => Some(arg0()),

            // QUERIES, SCALARS, VALUE TYPES, & UNIT RETURNS (V1 & V2)
            // Token V1 ID structs, withdraw capabilities, scalar balances, properties.
            // Token V2 scalars, strings, indices. 
            // All of these safely return Bottom because they do not carry pointer semantics.
            ("token", _) => Some(Provenance::Bottom),

            // =================================================================
            // 20. GUID / EVENT — value type helpers, no storage addresses.
            // =================================================================
            ("guid", "create") |
            ("guid", "create_id") |
            ("guid", "id") |
            ("guid", "creator_address") |
            ("guid", "creation_num") => Some(Provenance::Bottom),

            ("event", "new_event_handle") |
            ("event", "emit") |
            ("event", "emit_event") |
            ("event", "destroy_handle") |
            ("event", "counter") => Some(Provenance::Bottom),
 
            // =================================================================
            // 22. TRANSACTION_VALIDATION
            //
            // The prologue and epilogue are called by the VM for every transaction.
            // They are NOT in the user's call graph — the VM calls them directly
            // around the entry function. So the interpreter never walks them as
            // callees of the entry function.
            //
            // HOWEVER: the interpreter IS asked to analyze them when the entry
            // function IS the prologue or epilogue (which doesn't happen in normal
            // benchmarks). And occasionally framework functions call into
            // prologue_common helpers.
            //
            // What the prologue actually reads (from source):
            //   - timestamp::CurrentTimeMicroseconds@0x1  (now_seconds check)
            //   - chain_id::ChainId@0x1                  (chain id check)
            //   - Account@sender                         (seq#, auth key)
            //   - FungibleStore@sender_pfs               (balance check for gas)
            //
            // What the epilogue actually writes:
            //   - Account@sender                         (increment_sequence_number)
            //   - FungibleStore@sender_pfs / CollectedFeesPerBlock  (via transaction_fee)
            //
            // These are the resources the manual STEP 1A.5 patches inject.
            // Having these handlers lets the interpreter correctly model any
            // framework code that calls into transaction_validation helpers.
            //
            // All functions are unit-returning or bool; body walk handles globals.
            // =================================================================
 
            ("transaction_validation", "prologue_common") |
            ("transaction_validation", "check_for_replay_protection_regular_txn") |
            ("transaction_validation", "check_for_replay_protection_orderless_txn") |
            ("transaction_validation", "multi_agent_common_prologue") |
            ("transaction_validation", "script_prologue") |
            ("transaction_validation", "script_prologue_extended") |
            ("transaction_validation", "multi_agent_script_prologue") |
            ("transaction_validation", "multi_agent_script_prologue_extended") |
            ("transaction_validation", "fee_payer_script_prologue") |
            ("transaction_validation", "fee_payer_script_prologue_extended") |
            ("transaction_validation", "unified_prologue") |
            ("transaction_validation", "unified_prologue_v2") |
            ("transaction_validation", "unified_prologue_fee_payer") |
            ("transaction_validation", "unified_prologue_fee_payer_v2") |
            ("transaction_validation", "epilogue") |
            ("transaction_validation", "epilogue_extended") |
            ("transaction_validation", "epilogue_gas_payer") |
            ("transaction_validation", "epilogue_gas_payer_extended") |
            ("transaction_validation", "unified_epilogue") |
            ("transaction_validation", "unified_epilogue_v2") |
            ("transaction_validation", "grant_gas_permission") |
            ("transaction_validation", "revoke_gas_permission") |
            ("transaction_validation", "skip_auth_key_check") |
            ("transaction_validation", "skip_gas_payment") |
            ("transaction_validation", "allow_missing_txn_authentication_key") => Some(Provenance::Bottom),
 
            // =================================================================
            // 23. TRANSACTION_FEE
            //
            // Called from epilogue_gas_payer_extended:
            //   burn_fee(gas_payer, burn_amount)
            //     → burns from FungibleStore@gas_payer_pfs via aptos_account::burn_from_fungible_store_for_gas
            //     or coin::burn_from. Unit return.
            //   mint_and_refund(account, mint_amount)
            //     → mints APT and deposits to FungibleStore@account_pfs. Unit return.
            //   collect_fee / process_collected_fees — framework-level, touch
            //     CollectedFeesPerBlock@0x1. Unit return.
            //   store_aptos_coin_burn_cap / store_aptos_coin_mint_cap — genesis only.
            // =================================================================
 
            ("transaction_fee", "burn_fee") |
            ("transaction_fee", "mint_and_refund") |
            ("transaction_fee", "collect_fee") |
            ("transaction_fee", "process_collected_fees") |
            ("transaction_fee", "store_aptos_coin_burn_cap") |
            ("transaction_fee", "store_aptos_coin_mint_cap") |
            ("transaction_fee", "emit_fee_statement") |
            ("transaction_fee", "initialize_fee_collection_and_distribution") => Some(Provenance::Bottom),
 
            // =================================================================
            // 24. TIMESTAMP / CHAIN_ID / CHAIN_STATUS
            //
            // Read-only singletons at 0x1. The interpreter records borrow_global
            // calls via body walk when it walks timestamp::now_seconds etc.
            // Return values are scalars — u64 for timestamps, u8 for chain_id.
            // =================================================================
 
            ("timestamp", "now_microseconds") |
            ("timestamp", "now_seconds") |
            ("timestamp", "update_global_time") => Some(Provenance::Bottom),
 
            ("chain_id", "get") => Some(Provenance::Bottom),
 
            ("chain_status", "is_genesis") |
            ("chain_status", "is_operating") |
            ("chain_status", "assert_genesis") |
            ("chain_status", "assert_operating") => Some(Provenance::Bottom),
 
            // =================================================================
            // 25. NONCE_VALIDATION
            //
            // check_and_insert_nonce(sender, nonce, expiration_time) → bool
            //   Reads/writes a nonce table at sender's address.
            //   The interpreter walks the body for globals. Return Bottom.
            // =================================================================
 
            ("nonce_validation", "check_and_insert_nonce") => Some(Provenance::Bottom),
 
            // =================================================================
            // 26. ACCOUNT_ABSTRACTION / SYSTEM_ADDRESSES / MISC FRAMEWORK
            //
            // account_abstraction — called during prologue for AA txns.
            // system_addresses — pure address checks, no storage.
            // =================================================================
 
            ("account_abstraction", "using_dispatchable_authenticator") |
            ("account_abstraction", "using_native_authenticator") |
            ("account_abstraction", "dispatchable_authenticate") => Some(Provenance::Bottom),
 
            ("system_addresses", "assert_aptos_framework") |
            ("system_addresses", "assert_core_resource") |
            ("system_addresses", "assert_vm") |
            ("system_addresses", "is_aptos_framework_address") |
            ("system_addresses", "is_core_resource_address") => Some(Provenance::Bottom),
 
            // string module helpers that appear in framework call graphs.
            ("string", "utf8") |
            ("string", "bytes") |
            ("string", "length") |
            ("string", "append") |
            ("string", "is_empty") => Some(Provenance::Bottom),
 
            // math64 / math128 — pure scalar arithmetic, no pointers.
            ("math64", "min") | ("math64", "max") |
            ("math128", "min") | ("math128", "max") => Some(Provenance::Bottom),

            // =================================================================
            // 27. APTOS TOKEN V2 (aptos_token)
            // =================================================================
            
            // BORROWS & EXTRACTORS
            // Returning Field(arg0) because they extract the inner Token/Collection data from the Object.
            ("aptos_token", "borrow") |
            ("aptos_token", "borrow_collection") |
            ("aptos_token", "authorized_borrow") |
            ("aptos_token", "authorized_borrow_collection") => Some(Provenance::Field(Box::new(arg0()))),

            // OBJECT & REF CONSTRUCTORS
            // Functions that derive or create an Object/ConstructorRef.
            // The address is derived from the creator/signer, which is Arg 0.
            ("aptos_token", "create_collection_object") |
            ("aptos_token", "mint_token_object") |
            ("aptos_token", "mint_soul_bound_token_object") |
            ("aptos_token", "mint_internal") |
            ("aptos_token", "collection_object") |
            ("aptos_token", "create_collection_helper") |
            ("aptos_token", "mint_helper") => Some(arg0()),

            // QUERIES, SCALARS, & UNIT RETURNS
            // All bool checks (is_burnable, is_mutable_name, etc.), unit returns, and tests.
            // These do not carry pointer semantics, so they return Bottom safely.
            ("aptos_token", _) => Some(Provenance::Bottom),

            // =================================================================
            // 28. COLLECTION (Token V2)
            // =================================================================
            
            // BORROWS & EXTRACTORS
            // Returning Field(arg0) because they extract the inner Collection data from the Object/Ref.
            ("collection", "borrow") |
            ("collection", "borrow_mut") => Some(Provenance::Field(Box::new(arg0()))),

            // OBJECT & REF CONSTRUCTORS
            // Functions that derive or create a Collection Object, Address, or ConstructorRef.
            // The address is derived from the creator/signer, which is Arg 0.
            ("collection", "create_fixed_collection") |
            ("collection", "create_fixed_collection_as_owner") |
            ("collection", "create_unlimited_collection") |
            ("collection", "create_unlimited_collection_as_owner") |
            ("collection", "create_untracked_collection") |
            ("collection", "create_collection_internal") |
            ("collection", "create_collection_address") |
            ("collection", "create_collection_helper") |
            ("collection", "create_fixed_collection_helper") |
            ("collection", "create_unlimited_collection_as_owner_helper") |
            ("collection", "create_token") |
            ("collection", "generate_mutator_ref") => Some(arg0()),

            // QUERIES, SCALARS, & UNIT RETURNS
            // Includes supply increments/decrements, counts, descriptions, and string queries.
            // These do not carry pointer semantics, so they return Bottom safely.
            ("collection", _) => Some(Provenance::Bottom),

            // =================================================================
            // DEX POSITION & TRACKER GETTERS
            // These return &mut references to nested structs inside Maps/Tables
            // =================================================================
            ("perp_positions", "get_market") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // COLLATERAL & VAULT GETTERS
            // These extract the FungibleStore Objects from the subaccount
            // =================================================================
            ("collateral_balance_sheet", "get_primary_balance_from_status") |
            ("collateral_balance_sheet", "get_secondary_balance_from_status") |
            ("collateral_balance_sheet", "get_total_balance_from_status") |
            ("collateral_balance_sheet", "get_liquidation_burn_fn") |
            ("collateral_balance_sheet", "get_secondary_assets_value_in_primary") |
            ("accounts_collateral", "get_cross_total_collateral_value") |
            ("accounts_collateral", "get_isolated_position_total_collateral_value") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // DEX SCALAR / VALUE GETTERS (Return u64, bool, or copied structs)
            // =================================================================
            ("chainlink_state", "get_latest_price") |
            ("chainlink_state", "get_oracle_market_status") |
            ("clearinghouse_perp", "get_adjusted_size_for_open_interest_cap") |
            ("internal_oracle_state", "get_oracle_market_status") |
            ("market_types", "get_callback_result") |
            ("market_types", "get_place_maker_order_actions") |
            ("perp_market_config", "get_oracle_data") |
            ("perp_positions", "get_position_contribution_for_cache") |
            ("perp_positions", "get_position_funding_cost_and_index") |
            ("perp_positions", "get_position_funding_cost_and_index_impl") |
            ("perp_positions", "get_position_size_and_is_long") |
            ("position_update", "get_fee_and_volume_delta") |
            ("position_update", "get_pnl_and_funding_for_decrease") |
            ("work_unit_utils", "get_finish_or_abort_work_units") |
            ("order_book_types", "get_trigger_condition_indices") => {
                Some(Provenance::Bottom)
            },

            // =================================================================
            // DEX ADDRESS & SIGNER GETTERS
            // =================================================================
            ("dex_accounts", "get_subaccount_signer_if_owner_or_delegated_for_perp_trading") |
            ("dex_accounts", "subaccount_address") |
            ("dex_accounts", "get_subaccount_address") |
            ("dex_accounts", "get_subaccount_address_from_id") |
            ("dex_accounts", "get_subaccount_address_by_id") |
            ("dex_accounts", "get_subaccount_address_from_primary") |
            ("dex_accounts", "get_subaccount_id") |
            ("adl_tracker", "get_next_adl_address") |
            ("perp_positions", "get_primary_account_addr") => {
                Some(arg0())
            },

            // =================================================================
            // DEX REFERENCE GETTERS (Return &mut or &)
            // =================================================================
            ("backstop_liquidation", "get_dlp_position_snapshot") |
            ("bulk_order_utils", "match_order_and_get_next_from_bulk_order") |
            ("internal_oracle_state", "get_internal_source_data") |
            ("oracle", "get_composite_oracle_data") |
            ("oracle", "get_composite_oracle_data_from_fields") |
            ("oracle", "get_oracle_data") |
            ("oracle", "get_single_feed_oracle_data") |
            ("oracle", "get_single_oracle_market_status") |
            ("pending_order_tracker", "get_all_tp_sls_for_event") |
            ("pending_order_tracker", "get_fixed_sized_tp_sl") |
            ("perp_positions", "get_perp_position") |
            ("perp_positions", "get_perp_position_mut") |
            ("perp_positions", "get_position") |
            ("perp_positions", "get_position_mut") |
            ("position_tp_sl", "get_fixed_sized_tp_sl") |
            ("position_tp_sl_tracker", "get_pending_tp_sl") |
            ("price_management", "get_market_state") |
            ("price_management", "get_market_state_for_position_status") |
            ("price_management", "get_market_state_for_position_status_from_price") |
            ("price_time_index", "get_single_match_result") |
            ("tp_sl_utils", "validate_and_get_child_tp_sl_orders") |
            ("market_types", "get_market") |
            ("market_types", "get_market_mut") |
            ("market_types", "get_market_state_mut") |
            ("trading_fees_manager", "get_maker_fees_and_config") |
            ("trading_fees_manager", "get_taker_fees_and_config") |
            ("trading_fees_manager", "get_fees_and_config") |
            ("trading_fees_manager", "get_fee_config") |
            ("order_book", "get_next_maker_order") |
            ("order_book", "get_next_order") |
            ("order_book", "get_order") |
            ("single_order_book", "get_next_maker_order") |
            ("single_order_book", "get_next_order") |
            ("single_order_book", "get_maker_order") |
            ("single_order_book", "get_maker_order_mut") |
            ("single_order_book", "get_order") |
            ("single_order_book", "get_order_mut") |
            ("bulk_order_book", "get_next_maker_order") |
            ("bulk_order_book", "get_next_order") |
            ("bulk_order_book", "get_order") |
            ("bulk_order_book", "get_order_mut") => {
                Some(Provenance::Field(Box::new(arg0())))
            },

            // =================================================================
            // COLLATERAL & VAULT GETTERS
            // =================================================================
            ("collateral_balance_sheet", "get_collateral_status") |
            ("collateral_balance_sheet", "get_primary_balance_from_status") |
            ("collateral_balance_sheet", "get_secondary_balance_from_status") |
            ("collateral_balance_sheet", "get_total_balance_from_status") |
            ("collateral_balance_sheet", "get_liquidation_burn_fn") |
            ("collateral_balance_sheet", "get_secondary_assets_value_in_primary") |
            ("accounts_collateral", "get_cross_total_collateral_value") |
            ("accounts_collateral", "get_isolated_position_total_collateral_value") |
            ("accounts_collateral", "get_collateral_status_mut") |
            ("accounts_collateral", "get_collateral_status") |
            ("accounts_collateral", "get_collateral_status_from_subaccount") => {
                Some(Provenance::Field(Box::new(arg0())))
            },
 
            // Not recognised — caller falls through to Strategy 2 / Strategy 3 and logs.
            _ => None,
        }
    }

    /// Manually injects Write dependencies for native/framework functions that mutate
    /// their arguments but either have no bytecode or return `()`.
    fn handle_native_mutations(&mut self, callee: &crate::analyzer::FunctionID, args: &Vec<AbstractValue>) {
        let arg0 = || args.get(0).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);
        let arg1 = || args.get(1).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);
        let arg2 = || args.get(2).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);
        // Used for functions where the mutated reference isn't the first or second argument
        let arg3 = || args.get(3).map(|a| a.provenance.clone()).unwrap_or(Provenance::Bottom);

        match (callee.module.as_str(), callee.name.as_str()) {
            // Tables and Maps (The core of the Order Book!)
            ("table", "add") | ("table", "add_box") |
            ("table", "remove") | ("table", "remove_box") | ("table", "upsert") |
            ("smart_table", "add") | ("smart_table", "remove") | ("smart_table", "upsert") |
            ("table_with_length", "add") | ("table_with_length", "add_box") |
            ("table_with_length", "remove") | ("table_with_length", "remove_box") |
            
            // =================================================================
            // ORDERED MAP MUTATIONS
            // =================================================================
            
            // Functions taking `self: &mut OrderedMap<K, V>` as Arg 0.
            ("ordered_map", "add") |
            ("ordered_map", "upsert") |
            ("ordered_map", "remove") |
            ("ordered_map", "remove_or_none") |
            ("ordered_map", "modify_or_add") |
            ("ordered_map", "modify_if_present") |
            ("ordered_map", "replace_key_inplace") |
            ("ordered_map", "add_all") |
            ("ordered_map", "upsert_all") |
            ("ordered_map", "append") |
            ("ordered_map", "append_disjoint") |
            ("ordered_map", "append_impl") |
            ("ordered_map", "trim") |
            ("ordered_map", "pop_front") |
            ("ordered_map", "pop_back") => {
                self.handle_write_to_provenance(&arg0());
            },

            // Functions taking `map: &mut OrderedMap<K, V>` as Arg 1 (Iterators).
            ("ordered_map", "iter_borrow_mut") |
            ("ordered_map", "iter_remove") |
            ("ordered_map", "iter_replace") |
            ("ordered_map", "iter_add") => {
                // The map being mutated is passed as Arg 1 alongside the Iterator in Arg 0
                self.handle_write_to_provenance(&arg1());
            },

            // =================================================================
            // BIG ORDERED MAP MUTATIONS
            // =================================================================
            
            // Functions taking `self: &mut BigOrderedMap<K, V>` or mutating via an IteratorPtr.
            ("big_ordered_map", "allocate_spare_slots") |
            ("big_ordered_map", "add") |
            ("big_ordered_map", "upsert") |
            ("big_ordered_map", "remove") |
            ("big_ordered_map", "remove_or_none") |
            ("big_ordered_map", "modify") |
            ("big_ordered_map", "modify_and_return") |
            ("big_ordered_map", "modify_or_add") |
            ("big_ordered_map", "modify_if_present") |
            ("big_ordered_map", "modify_if_present_and_return") |
            ("big_ordered_map", "add_all") |
            ("big_ordered_map", "pop_front") |
            ("big_ordered_map", "pop_back") |
            ("big_ordered_map", "iter_modify") |
            ("big_ordered_map", "iter_remove") |
            ("big_ordered_map", "add_or_upsert_impl") |
            ("big_ordered_map", "validate_dynamic_size_and_init_max_degrees") |
            ("big_ordered_map", "validate_static_size_and_init_max_degrees") |
            ("big_ordered_map", "validate_size_and_init_max_degrees") |
            ("big_ordered_map", "replace_root") |
            ("big_ordered_map", "add_at") |
            ("big_ordered_map", "update_key") |
            ("big_ordered_map", "remove_at") => {
                // All state changes target the Map or Iterator explicitly passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // AGGREGATOR MUTATIONS
            // =================================================================
            
            // Functions taking `self: &mut Aggregator`, `&mut OptionalAggregator`, 
            // `&mut Integer`, or `&mut AggregatorFactory` as Arg 0.
            ("aggregator", "add") |
            ("aggregator", "sub") |
            ("aggregator_v2", "add") |
            ("aggregator_v2", "sub") |
            ("aggregator_v2", "try_add") |
            ("aggregator_v2", "try_sub") |
            ("aggregator_v2", "verify_aggregator_generic_add") |
            ("aggregator_v2", "verify_aggregator_generic_sub") |
            ("optional_aggregator", "add") |
            ("optional_aggregator", "sub") |
            ("optional_aggregator", "add_integer") |
            ("optional_aggregator", "sub_integer") |
            ("optional_aggregator", "switch") |
            ("aggregator_factory", "new_aggregator") => {
                // Aggregator deltas are commutative in Block-STM. Treating them as strict writes
                // forces the DAG to serialize them. By ignoring the write here, the resource 
                // remains in the `read_set` (from the MutBorrowGlobal), allowing perfect concurrency.
                //
                // self.handle_write_to_provenance(&arg0()); <--- No need for this anymore
            },

            // =================================================================
            // OPTION MUTATIONS
            // =================================================================
            
            // Functions taking `self: &mut Option<Element>` as Arg 0.
            ("option", "fill") |
            ("option", "extract") |
            ("option", "borrow_mut") |
            ("option", "swap") |
            ("option", "swap_or_fill") => {
                // Mutates the `&mut Option` passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // STORAGE SLOTS ALLOCATOR MUTATIONS
            // =================================================================
            
            // Functions taking `self: &mut StorageSlotsAllocator<T>` as Arg 0.
            ("storage_slots_allocator", "allocate_spare_slots") |
            ("storage_slots_allocator", "add") |
            ("storage_slots_allocator", "remove") |
            ("storage_slots_allocator", "reserve_slot") |
            ("storage_slots_allocator", "fill_reserved_slot") |
            ("storage_slots_allocator", "remove_and_reserve") |
            ("storage_slots_allocator", "free_reserved_slot") |
            ("storage_slots_allocator", "maybe_pop_from_reuse_queue") |
            ("storage_slots_allocator", "maybe_push_to_reuse_queue") |
            ("storage_slots_allocator", "next_slot_index") |
            ("storage_slots_allocator", "add_link") |
            ("storage_slots_allocator", "remove_link") => {
                // Mutates the `&mut StorageSlotsAllocator` passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // Events - ONLY emit_event and write_to_event_store take an EventHandle as arg0.
            // `event::emit` takes the payload `msg: T` directly and does NOT mutate it.
            ("event", "emit_event") | ("event", "write_to_event_store") => {
                let prov = arg0();
                self.handle_write_to_provenance(&prov);
            },

            // =================================================================
            // NEWLY ADDED NATIVE MUTATIONS
            // =================================================================

            // mem::swap<T>(left: &mut T, right: &mut T)
            ("mem", "swap") => {
                self.handle_write_to_provenance(&arg0());
                self.handle_write_to_provenance(&arg1());
            },

            // vector::move_range<T>(from: &mut vector<T>, removal_pos: u64, len: u64, to: &mut vector<T>, insert_pos: u64)
            ("vector", "move_range") => {
                self.handle_write_to_provenance(&arg0()); // `from` vector
                self.handle_write_to_provenance(&arg3()); // `to` vector
            },

            // =================================================================
            // VECTOR & SMART VECTOR MUTATIONS
            // =================================================================
            ("vector", "push_back") | ("vector", "pop_back") |
            ("vector", "remove") | ("vector", "swap_remove") | ("vector", "reverse") |
            ("smart_vector", "push_back") | ("smart_vector", "pop_back") |
            ("smart_vector", "remove") | ("smart_vector", "swap_remove") | ("smart_vector", "reverse") => {
                // arg0 is &mut vector<T>
                let prov = arg0();
                self.handle_write_to_provenance(&prov);
            },
            
            ("vector", "append") | ("smart_vector", "append") | ("smart_vector", "append_all") => {
                // v1 (arg0) gets mutated. v2 (arg1) is consumed/read.
                let prov = arg0();
                self.handle_write_to_provenance(&prov);
            },

            // =================================================================
            // APTOS ACCOUNT: TRANSFERS & DEPOSITS
            // =================================================================
            
            // transfer(source: &signer, to: address, amount: u64)
            // transfer_coins<CoinType>(from: &signer, to: address, amount: u64)
            // fungible_transfer_only(source: &signer, to: address, amount: u64)
            ("aptos_account", "transfer") |
            ("aptos_account", "transfer_coins") |
            ("aptos_account", "fungible_transfer_only") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(), // Proxy for global balance
                    address_source: Box::new(arg0()), // Sender is Arg 0
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Receiver is Arg 1
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },

            // transfer_fungible_assets(from: &signer, metadata: Object<Metadata>, to: address, amount: u64)
            ("aptos_account", "transfer_fungible_assets") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Sender is Arg 0
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg2()), // Receiver is Arg 2
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },

            // deposit_coins<CoinType>(to: address, coins: Coin<CoinType>)
            // deposit_fungible_assets(to: address, fa: FungibleAsset)
            ("aptos_account", "deposit_coins") |
            ("aptos_account", "deposit_fungible_assets") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Target is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // batch_transfer, batch_transfer_coins, batch_transfer_fungible_assets 
            // (Note: Since receivers are in a vector, statically extracting addresses safely is complex. 
            // We primarily track the sender's balance deduction here).
            ("aptos_account", "batch_transfer") |
            ("aptos_account", "batch_transfer_coins") |
            ("aptos_account", "batch_transfer_fungible_assets") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Sender is Arg 0
                };
                self.handle_write_to_provenance(&prov_from);
            },

            // =================================================================
            // ACCOUNT: GLOBAL STATE MUTATIONS
            // =================================================================
            
            // Functions that modify the `Account` resource at the given address (arg0).
            // Includes account creation, sequence number bumps, key rotations, and capabilities.
            ("account", "create_account") |
            ("account", "create_account_if_does_not_exist") |
            ("account", "create_account_unchecked") |
            ("account", "increment_sequence_number") |
            ("account", "set_sequence_number") |
            ("account", "register_coin") |
            ("account", "rotate_authentication_key_internal") |
            ("account", "rotate_authentication_key_call") |
            ("account", "rotate_authentication_key") |
            ("account", "rotate_authentication_key_from_public_key") |
            ("account", "upsert_ed25519_backup_key_on_keyless_account") |
            ("account", "rotate_authentication_key_with_rotation_capability") |
            ("account", "offer_rotation_capability") |
            ("account", "revoke_rotation_capability") |
            ("account", "revoke_any_rotation_capability") |
            ("account", "offer_signer_capability") |
            ("account", "revoke_signer_capability") |
            ("account", "revoke_any_signer_capability") |
            ("account", "set_originating_address") |
            ("account", "set_signer_capability_offer") |
            ("account", "set_rotation_capability_offer") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::account::Account".to_string(),
                    address_source: Box::new(arg0()), // Target address/signer is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // =================================================================
            // FUNGIBLE ASSET: TRANSFERS (SENDER -> RECEIVER)
            // =================================================================
            
            // transfer(sender: &signer, from: Object<T>, to: Object<T>, amount: u64)
            // transfer_with_ref(self: &TransferRef, from: Object<T>, to: Object<T>, amount: u64)
            // dispatchable_fungible_asset versions share exact parameter indices.
            ("fungible_asset", "transfer") | 
            ("fungible_asset", "transfer_with_ref") |
            ("dispatchable_fungible_asset", "transfer") | 
            ("dispatchable_fungible_asset", "transfer_assert_minimum_deposit") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // 'from' store is Arg 1
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg2()), // 'to' store is Arg 2
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
                
                // Track mutations on the references themselves
                self.handle_write_to_provenance(&arg1());
                self.handle_write_to_provenance(&arg2());
            },

            // =================================================================
            // FUNGIBLE ASSET: DEPOSITS
            // =================================================================
            
            // deposit(store: Object<T>, fa: FungibleAsset)
            // unchecked_deposit(store_addr: address, fa: FungibleAsset)
            // unchecked_deposit_with_no_events(store_addr: address, fa: FungibleAsset)
            // set_frozen_flag_internal(store: Object<T>, frozen: bool)
            ("fungible_asset", "deposit") | 
            ("fungible_asset", "unchecked_deposit") | 
            ("fungible_asset", "unchecked_deposit_with_no_events") |
            ("fungible_asset", "set_frozen_flag_internal") |
            ("dispatchable_fungible_asset", "deposit") | 
            ("dispatchable_fungible_asset", "dispatchable_deposit") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Store/Store_addr is Arg 0
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0());
            },

            // deposit_with_ref(self: &TransferRef, store: Object<T>, fa: FungibleAsset)
            ("fungible_asset", "deposit_with_ref") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Store is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg1());
            },

            // =================================================================
            // FUNGIBLE ASSET: WITHDRAWS & BURNS FROM A STORE
            // =================================================================

            // withdraw(owner: &signer, store: Object<T>, amount: u64)
            // withdraw_with_ref(self: &TransferRef, store: Object<T>, amount: u64)
            // burn_from(self: &BurnRef, store: Object<T>, amount: u64)
            // address_burn_from_for_gas(self: &BurnRef, store_addr: address, amount: u64)
            // set_frozen_flag(self: &TransferRef, store: Object<T>, frozen: bool)
            // upgrade_store_to_concurrent(owner: &signer, store: Object<T>)
            ("fungible_asset", "withdraw") | 
            ("fungible_asset", "withdraw_with_ref") |
            ("fungible_asset", "burn_from") | 
            ("fungible_asset", "address_burn_from_for_gas") |
            ("fungible_asset", "set_frozen_flag") |
            ("fungible_asset", "upgrade_store_to_concurrent") |
            ("dispatchable_fungible_asset", "withdraw")  => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Store/Store_addr is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg1());
            },

            // unchecked_withdraw(store_addr: address, amount: u64)
            // unchecked_withdraw_with_no_events(store_addr: address, amount: u64)
            ("fungible_asset", "unchecked_withdraw") |
            ("fungible_asset", "unchecked_withdraw_with_no_events") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Store_addr is Arg 0
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // FUNGIBLE ASSET: MINTS, SUPPLY, METADATA & GLOBALS
            // =================================================================

            // mint_to(self: &MintRef, store: Object<T>, amount: u64)
            ("fungible_asset", "mint_to") => {
                let store_prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Store is Arg 1
                };
                let supply_prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::Supply".to_string(),
                    address_source: Box::new(arg0()), // Supply tracking derived from MintRef (Arg 0)
                };
                self.handle_write_to_provenance(&store_prov);
                self.handle_write_to_provenance(&supply_prov);
                self.handle_write_to_provenance(&arg1());
                self.handle_write_to_provenance(&arg0());
            },

            // mint(self: &MintRef, amount: u64)
            // mint_internal(metadata: Object<Metadata>, amount: u64)
            // burn(self: &BurnRef, fa: FungibleAsset)
            // upgrade_to_concurrent(ref: &ExtendRef)
            // remove_store(delete_ref: &DeleteRef)
            // set_untransferable(constructor_ref: &ConstructorRef)
            // mutate_metadata(self: &MutateMetadataRef, ...)
            // ensure_store_upgraded_to_concurrent_internal(fungible_store_address: address)
            ("fungible_asset", "mint") |
            ("fungible_asset", "mint_internal") |
            ("fungible_asset", "burn") |
            ("fungible_asset", "upgrade_to_concurrent") |
            ("fungible_asset", "remove_store") |
            ("fungible_asset", "set_untransferable") |
            ("fungible_asset", "mutate_metadata") |
            ("fungible_asset", "ensure_store_upgraded_to_concurrent_internal") => {
                // All of these mutate Global resources linked exclusively to Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // FUNGIBLE ASSET: PERMISSIONS
            // =================================================================

            // grant_permission_by_store(master: &signer, permissioned: &signer, store: Object<T>, amount: u64)
            // grant_permission_by_address(master: &signer, permissioned: &signer, store_address: address, amount: u64)
            // refill_permission(permissioned: &signer, amount: u64, store_address: address)
            ("fungible_asset", "grant_permission_by_store") |
            ("fungible_asset", "grant_permission_by_address") |
            ("fungible_asset", "refill_permission") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg2()), // Store is Arg 2
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg2());
            },

            // dispatchable_withdraw(store: Object<T>, amount: u64, transfer_ref: &TransferRef, function: &FunctionInfo)
            ("dispatchable_fungible_asset", "dispatchable_withdraw") => {
                // IMPORTANT DIFFERENCE: No owner signer arg! Store is Arg 0.
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), 
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0());
            },
            
            // =================================================================
            // DISPATCHABLE FUNGIBLE ASSET: METADATA SETUP
            // =================================================================

            // register_dispatch_functions(constructor_ref: &ConstructorRef, ...)
            // register_derive_supply_dispatch_function(constructor_ref: &ConstructorRef, ...)
            ("dispatchable_fungible_asset", "register_dispatch_functions") |
            ("dispatchable_fungible_asset", "register_derive_supply_dispatch_function") => {
                self.handle_write_to_provenance(&arg0()); // Modifies the object wrapped by the ref
            },

            // =================================================================
            // PRIMARY FUNGIBLE STORE: TRANSFERS
            // =================================================================
            
            // transfer(sender: &signer, metadata: Object<T>, recipient: address, amount: u64)
            // transfer_assert_minimum_deposit(sender: &signer, metadata: Object<T>, recipient: address, amount: u64, expected: u64)
            ("primary_fungible_store", "transfer") | 
            ("primary_fungible_store", "transfer_assert_minimum_deposit") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Sender's derived store (Arg 0)
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg2()), // Recipient's derived store (Arg 2)
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },

            // transfer_with_ref(transfer_ref: &TransferRef, from: address, to: address, amount: u64)
            ("primary_fungible_store", "transfer_with_ref") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // 'from' address derived store (Arg 1)
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg2()), // 'to' address derived store (Arg 2)
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
                self.handle_write_to_provenance(&arg0()); // Mutates the TransferRef
            },

            // =================================================================
            // PRIMARY FUNGIBLE STORE: DEPOSITS & MINTS
            // =================================================================
            
            // deposit(owner: address, fa: FungibleAsset)
            // deposit_with_signer(owner: &signer, fa: FungibleAsset)
            ("primary_fungible_store", "deposit") |
            ("primary_fungible_store", "deposit_with_signer") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Owner is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // deposit_with_ref(transfer_ref: &TransferRef, owner: address, fa: FungibleAsset)
            // mint(mint_ref: &MintRef, owner: address, amount: u64)
            ("primary_fungible_store", "deposit_with_ref") |
            ("primary_fungible_store", "mint") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Owner is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); // Mutates the Ref
            },

            // =================================================================
            // PRIMARY FUNGIBLE STORE: WITHDRAWS, BURNS, & PERMISSIONS
            // =================================================================

            // withdraw(owner: &signer, metadata: Object<T>, amount: u64)
            // grant_permission(master: &signer, permissioned: &signer, metadata: Object<T>, amount: u64)
            // grant_apt_permission(master: &signer, permissioned: &signer, amount: u64)
            ("primary_fungible_store", "withdraw") |
            ("primary_fungible_store", "grant_permission") |
            ("primary_fungible_store", "grant_apt_permission") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Owner/Master is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // withdraw_with_ref(transfer_ref: &TransferRef, owner: address, amount: u64)
            // burn(burn_ref: &BurnRef, owner: address, amount: u64)
            // set_frozen_flag(transfer_ref: &TransferRef, owner: address, frozen: bool)
            ("primary_fungible_store", "withdraw_with_ref") |
            ("primary_fungible_store", "burn") |
            ("primary_fungible_store", "set_frozen_flag") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg1()), // Owner is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); // Mutates the Ref
            },

            // may_be_unburn(owner: &signer, store: Object<FungibleStore>)
            ("primary_fungible_store", "may_be_unburn") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // Owner is Arg 0
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg1()); // Mutates the explicitly passed store
            },

            // =================================================================
            // COIN: TRANSFERS
            // =================================================================
            
            // transfer(from: &signer, to: address, amount: u64)
            ("coin", "transfer") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x1::coin::CoinStore".to_string(),
                    address_source: Box::new(arg0()), // from (signer) is Arg 0
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x1::coin::CoinStore".to_string(),
                    address_source: Box::new(arg1()), // to (address) is Arg 1
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },

            // =================================================================
            // COIN: DEPOSITS
            // =================================================================
            
            // deposit(account_addr: address, coin: Coin<CoinType>)
            // deposit_with_signer(account: &signer, coin: Coin<CoinType>)
            // deposit_for_gas_fee(account_addr: address, coin: Coin<CoinType>)
            ("coin", "deposit") |
            ("coin", "deposit_with_signer") |
            ("coin", "deposit_for_gas_fee") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::coin::CoinStore".to_string(),
                    address_source: Box::new(arg0()), // Target address or signer is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // =================================================================
            // COIN: WITHDRAWS, BURNS, & ACCOUNT MODIFICATIONS
            // =================================================================
            
            // withdraw(account: &signer, amount: u64)
            // burn_from(account_addr: address, amount: u64, burn_cap: &BurnCapability<CoinType>)
            // burn_from_for_gas(account_addr: address, amount: u64, burn_cap: &BurnCapability<CoinType>)
            // freeze_coin_store(account_addr: address, _freeze_cap: &FreezeCapability<CoinType>)
            // unfreeze_coin_store(account_addr: address, _freeze_cap: &FreezeCapability<CoinType>)
            // register(account: &signer)
            // migrate_to_fungible_store(account: &signer)
            ("coin", "withdraw") |
            ("coin", "burn_from") |
            ("coin", "burn_from_for_gas") |
            ("coin", "freeze_coin_store") |
            ("coin", "unfreeze_coin_store") |
            ("coin", "register") |
            ("coin", "migrate_to_fungible_store") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::coin::CoinStore".to_string(),
                    address_source: Box::new(arg0()), // Source address or signer is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // =================================================================
            // COIN: VALUE MUTATIONS
            // =================================================================
            
            // extract(coin: &mut Coin<CoinType>, amount: u64)
            // extract_all(coin: &mut Coin<CoinType>)
            // merge(dst_coin: &mut Coin<CoinType>, source_coin: Coin<CoinType>)
            ("coin", "extract") |
            ("coin", "extract_all") |
            ("coin", "merge") => {
                // Mutates the `&mut Coin` instance passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // FEATURES: BYTE VECTOR MUTATIONS
            // =================================================================
            
            // set(features: &mut vector<u8>, feature: u64, include: bool)
            // apply_diff(features: &mut vector<u8>, enable: vector<u64>, disable: vector<u64>)
            ("features", "set") |
            ("features", "apply_diff") => {
                // Mutates the `features: &mut vector<u8>` passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // OBJECT: TRANSFERS & STATE MUTATIONS
            // =================================================================
            
            // transfer_call(owner: &signer, object: address, to: address)
            // transfer<T>(owner: &signer, object: Object<T>, to: address)
            // transfer_raw(owner: &signer, object: address, to: address)
            // transfer_to_object<O, T>(owner: &signer, object: Object<O>, to: Object<T>)
            // burn<T>(owner: &signer, object: Object<T>)
            // unburn<T>(original_owner: &signer, object: Object<T>)
            // burn_object_with_transfer<T>(owner: &signer, object: Object<T>)
            ("object", "transfer_call") |
            ("object", "transfer") |
            ("object", "transfer_raw") |
            ("object", "transfer_to_object") |
            ("object", "burn") |
            ("object", "unburn") |
            ("object", "burn_object_with_transfer") => {
                // Modifies ObjectCore at the object's address (Arg 1)
                let prov = Provenance::Global {
                    resource_type: "0x1::object::ObjectCore".to_string(),
                    address_source: Box::new(arg1()), 
                };
                self.handle_write_to_provenance(&prov);
            },

            // transfer_with_ref(self: LinearTransferRef, to: address)
            ("object", "transfer_with_ref") => {
                // Modifies ObjectCore at the object's address derived from the ref (Arg 0)
                let prov = Provenance::Global {
                    resource_type: "0x1::object::ObjectCore".to_string(),
                    address_source: Box::new(arg0()), 
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); // Also consumes/mutates the Ref
            },

            // grant_permission<T>(master: &signer, permissioned_signer: &signer, object: Object<T>)
            ("object", "grant_permission") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::object::ObjectCore".to_string(),
                    address_source: Box::new(arg2()), // Object is Arg 2
                };
                self.handle_write_to_provenance(&prov);
            },

            // grant_permission_with_transfer_ref(permissioned_signer: &signer, ref: &TransferRef)
            ("object", "grant_permission_with_transfer_ref") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::object::ObjectCore".to_string(),
                    address_source: Box::new(arg1()), // Ref is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg1()); // Mutates the ref
            },

            // delete(self: DeleteRef)
            // disable_ungated_transfer(self: &TransferRef)
            // set_untransferable(self: &ConstructorRef)
            // enable_ungated_transfer(self: &TransferRef)
            ("object", "delete") |
            ("object", "disable_ungated_transfer") |
            ("object", "set_untransferable") |
            ("object", "enable_ungated_transfer") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::object::ObjectCore".to_string(),
                    address_source: Box::new(arg0()), // Ref is Arg 0
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); // Mutates the Ref
            },

            // =================================================================
            // PERMISSIONED SIGNER: VALUE MUTATIONS
            // =================================================================
            
            // consume_capacity(perm: &mut StoredPermission, threshold: u256)
            // increase_capacity(perm: &mut StoredPermission, threshold: u256)
            // merge(lhs: &mut StoredPermission, rhs: StoredPermission)
            ("permissioned_signer", "consume_capacity") |
            ("permissioned_signer", "increase_capacity") |
            ("permissioned_signer", "merge") => {
                // Mutates the `&mut StoredPermission` passed as Arg 0
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // PERMISSIONED SIGNER: GLOBAL STATE MUTATIONS
            // =================================================================
            
            // authorize_increase(master: &signer, permissioned: &signer, ...)
            // authorize_unlimited(master: &signer, permissioned: &signer, ...)
            ("permissioned_signer", "authorize_increase") |
            ("permissioned_signer", "authorize_unlimited") => {
                let prov_master = Provenance::Global {
                    resource_type: "0x1::permissioned_signer::GrantedPermissionHandles".to_string(),
                    address_source: Box::new(arg0()), // Master is Arg 0
                };
                let prov_perm = Provenance::Global {
                    resource_type: "0x1::permissioned_signer::PermissionStorage".to_string(),
                    address_source: Box::new(arg1()), // Permissioned is Arg 1
                };
                self.handle_write_to_provenance(&prov_master);
                self.handle_write_to_provenance(&prov_perm);
            },

            // grant_unlimited_with_permissioned_signer(permissioned: &signer, ...)
            // increase_limit(permissioned: &signer, ...)
            // revoke_permission(permissioned: &signer, ...)
            ("permissioned_signer", "grant_unlimited_with_permissioned_signer") |
            ("permissioned_signer", "increase_limit") |
            ("permissioned_signer", "revoke_permission") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::permissioned_signer::PermissionStorage".to_string(),
                    address_source: Box::new(arg0()), // Permissioned is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },
            
            // grant_revoke_permission(master: &signer, permissioned: &signer)
            ("permissioned_signer", "grant_revoke_permission") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::permissioned_signer::PermissionStorage".to_string(),
                    address_source: Box::new(arg1()), // Permissioned is Arg 1
                };
                self.handle_write_to_provenance(&prov);
            },

            // revoke_all_handles(s: &signer)
            // revoke_permission_storage_address(s: &signer, permissions_storage_addr: address)
            ("permissioned_signer", "revoke_all_handles") |
            ("permissioned_signer", "revoke_permission_storage_address") => {
                let prov_master = Provenance::Global {
                    resource_type: "0x1::permissioned_signer::GrantedPermissionHandles".to_string(),
                    address_source: Box::new(arg0()), // Master is Arg 0
                };
                self.handle_write_to_provenance(&prov_master);
                
                // For revoke_permission_storage_address, the specific storage address is arg1
                if callee.name == "revoke_permission_storage_address" {
                    let prov_perm = Provenance::Global {
                        resource_type: "0x1::permissioned_signer::PermissionStorage".to_string(),
                        address_source: Box::new(arg1()), 
                    };
                    self.handle_write_to_provenance(&prov_perm);
                }
            },

            // =================================================================
            // TOKEN V1 & V2 MUTATIONS
            // =================================================================
            
            // Token V2: Object Mutations via Ref
            ("token", "set_description") |
            ("token", "set_name") |
            ("token", "set_uri") => {
                // Mutates the Token Object mapped by the MutatorRef (Arg 0)
                self.handle_write_to_provenance(&arg0());
            },

            // Overlap: `burn` exists in both V1 and V2
            // V1: burn(owner: &signer, ...) -> mutates TokenStore of owner (arg0)
            // V2: burn(burn_ref: BurnRef) -> mutates object mapped by BurnRef (arg0)
            ("token", "burn") => {
                self.handle_write_to_provenance(&arg0()); // Safely covers V2 Object mutation
                
                let prov_store = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg0()),
                };
                self.handle_write_to_provenance(&prov_store); // Covers V1 TokenStore mutation
            },

            // Token V1: Collections mutations (creator is Arg 0)
            ("token", "create_collection") |
            ("token", "create_collection_script") |
            ("token", "create_tokendata") |
            ("token", "create_token_script") |
            ("token", "mutate_collection_description") |
            ("token", "mutate_collection_uri") |
            ("token", "mutate_collection_maximum") |
            ("token", "mutate_tokendata_maximum") |
            ("token", "mutate_tokendata_uri") |
            ("token", "mutate_tokendata_royalty") |
            ("token", "mutate_tokendata_description") |
            ("token", "mutate_tokendata_property") |
            ("token", "burn_by_creator") => {
                let prov = Provenance::Global {
                    resource_type: "0x3::token::Collections".to_string(),
                    address_source: Box::new(arg0()), // Creator is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },
            
            // Token V1: TokenStore mutations (owner is Arg 0)
            ("token", "initialize_token_store") |
            ("token", "initialize_token_script") |
            ("token", "opt_in_direct_transfer") |
            ("token", "deposit_token") |
            ("token", "direct_deposit") |
            ("token", "direct_deposit_with_opt_in") |
            ("token", "withdraw_token") |
            ("token", "mint_script") |
            ("token", "mint_token") |
            ("token", "mutate_token_properties") |
            ("token", "mutate_one_token") => {
                let prov = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg0()), // Owner/Account is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // Token V1: mint_token_to (account: Arg 0, receiver: Arg 1)
            ("token", "mint_token_to") |
            ("token", "direct_transfer") |
            ("token", "direct_transfer_script") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg0()),
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg1()),
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },

            // Token V1: transfer (from: Arg 0, to: Arg 2)
            ("token", "transfer") => {
                let prov_from = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg0()),
                };
                let prov_to = Provenance::Global {
                    resource_type: "0x3::token::TokenStore".to_string(),
                    address_source: Box::new(arg2()),
                };
                self.handle_write_to_provenance(&prov_from);
                self.handle_write_to_provenance(&prov_to);
            },
            
            // Token V1: Value Type mutations
            // merge(dst_token: &mut Token, source_token: Token)
            // split(dst_token: &mut Token, amount: u64): Token
            ("token", "merge") |
            ("token", "split") => {
                self.handle_write_to_provenance(&arg0()); // &mut Token is Arg 0
            },

            // =================================================================
            // APTOS TOKEN V2 MUTATIONS
            // =================================================================

            // Object mutations where the target Object is Arg 1 (creator is Arg 0)
            ("aptos_token", "burn") |
            ("aptos_token", "freeze_transfer") |
            ("aptos_token", "unfreeze_transfer") |
            ("aptos_token", "set_description") |
            ("aptos_token", "set_name") |
            ("aptos_token", "set_uri") |
            ("aptos_token", "add_property") |
            ("aptos_token", "add_typed_property") |
            ("aptos_token", "remove_property") |
            ("aptos_token", "update_property") |
            ("aptos_token", "update_typed_property") |
            ("aptos_token", "set_collection_description") |
            ("aptos_token", "set_collection_royalties") |
            ("aptos_token", "set_collection_royalties_call") |
            ("aptos_token", "set_collection_uri") => {
                let prov = Provenance::Global {
                    resource_type: "0x4::aptos_token::AptosToken".to_string(), // Proxy for the Token/Collection state
                    address_source: Box::new(arg1()), // The object being modified is Arg 1
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg1()); // Mutates the object ref
            },

            // Creations where we approximate the new object's address space to the creator (Arg 0)
            ("aptos_token", "create_collection") |
            ("aptos_token", "mint") |
            ("aptos_token", "mint_soul_bound") => {
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // COLLECTION MUTATIONS
            // =================================================================

            // Object mutations via Ref (MutatorRef, ExtendRef, ConstructorRef) are Arg 0
            ("collection", "set_name") |
            ("collection", "set_description") |
            ("collection", "set_uri") |
            ("collection", "set_max_supply") |
            ("collection", "upgrade_to_concurrent") |
            ("collection", "downgrade_from_concurrent_for_test") |
            ("collection", "enable_ungated_transfer") => {
                let prov = Provenance::Global {
                    resource_type: "0x4::collection::Collection".to_string(),
                    address_source: Box::new(arg0()), // The capability reference is Arg 0
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); // Mutates the reference itself
            },

            // Mutations taking Object<Collection> directly as Arg 0
            ("collection", "increment_supply") |
            ("collection", "decrement_supply") => {
                let prov = Provenance::Global {
                    resource_type: "0x4::collection::Collection".to_string(),
                    address_source: Box::new(arg0()), // The Object is Arg 0
                };
                self.handle_write_to_provenance(&prov);
            },

            // Collection creation (approximates new object location via creator in Arg 0)
            ("collection", "create_fixed_collection") |
            ("collection", "create_fixed_collection_as_owner") |
            ("collection", "create_unlimited_collection") |
            ("collection", "create_unlimited_collection_as_owner") |
            ("collection", "create_untracked_collection") |
            ("collection", "create_collection_internal") => {
                self.handle_write_to_provenance(&arg0());
            },

            // =================================================================
            // BACKSTOP LIQUIDATOR TRANSFERS
            // =================================================================
            ("collateral_balance_sheet", "transfer_to_backstop_liquidator") |
            ("perp_positions", "transfer_balance_to_liquidator") |
            ("accounts_collateral", "transfer_balance_to_liquidator") |
            ("backstop_liquidation", "finalize_backstop_liquidation") => {
                let prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(arg0()), // User's subaccount/status
                };
                self.handle_write_to_provenance(&prov);
                self.handle_write_to_provenance(&arg0()); 
                
                // Add the explicit liquidator pool side of the transfer
                let liquidator_prov = Provenance::Global {
                    resource_type: "0x1::fungible_asset::FungibleStore".to_string(),
                    address_source: Box::new(Provenance::Const("STATIC_ADDR::backstop::liquidator".to_string())),
                };
                self.handle_write_to_provenance(&liquidator_prov);
            },

            _ => {}
        }
    }

    fn execute_pack(&mut self, field_count: usize) {
        let mut fields = Vec::new();
        for _ in 0..field_count {
            if let Some(val) = self.stack.pop() {
                fields.push(val);
            } else {
                fields.push(AbstractValue::new_bottom());
            }
        }
        fields.reverse();

        let field_provs: Vec<Provenance> = fields.into_iter().map(|f| f.provenance).collect();

        self.stack.push(AbstractValue {
            provenance: Provenance::Struct(field_provs),
        });
    }

    fn execute_unpack(&mut self, field_count: usize) {
        if let Some(struct_val) = self.stack.pop() {
            match struct_val.provenance {
                Provenance::Struct(field_provs) if field_provs.len() == field_count => {
                    for prov in field_provs {
                        self.stack.push(AbstractValue {
                            provenance: prov,
                        });
                    }
                },
                _ => {
                    for _ in 0..field_count {
                        self.stack.push(AbstractValue {
                            provenance: struct_val.provenance.clone(),
                        });
                    }
                }
            }
        } else {
            for _ in 0..field_count {
                self.stack.push(AbstractValue::new_bottom());
            }
        }
    }

    // --- RECURSIVE HELPERS ---

    fn handle_write_to_provenance(&mut self, prov: &Provenance) {

        // --- ADD THIS DEBUG BLOCK ---
        if matches!(prov, Provenance::Bottom) {
            let func_name = self.current_func_def.map(|def| {
                let handle = self.module.function_handle_at(def.function);
                self.module.identifier_at(handle.name).as_str()
            }).unwrap_or("unknown_func");
            
            eprintln!("[PROVENANCE-LEAK] Writing to Bottom in function: {}::{}", 
                self.module.self_id().name().as_str(), 
                func_name
            );
        }
        // ----------------------------

        // flatten() returns all root pointers — a Struct with [Param(0), Param(1)] produces
        // two entries, and we record a side-effect or write-set entry for each one.
        for flat_prov in prov.flatten() {
            // let func_name = self.current_func_def.map(|def| {
            //     let handle = self.module.function_handle_at(def.function);
            //     self.module.identifier_at(handle.name).as_str()
            // }).unwrap_or("unknown_func");

            // eprintln!(
            //     "[WRITE-TRACE] {}::{} inserting {:?}",
            //     self.module.self_id().name(),
            //     func_name,
            //     flat_prov
            // );
            
            let func_name = self.current_func_def.map(|def| {
                let handle = self.module.function_handle_at(def.function);
                self.module.identifier_at(handle.name).as_str()
            }).unwrap_or("unknown");

            if let Provenance::Global {
                resource_type,
                address_source,
            } = &flat_prov {

                if resource_type.contains("FungibleStore") &&
                matches!(address_source.as_ref(), Provenance::Global { .. })
                {
                    eprintln!(
                        "NESTED_FS_WRITE {:?}",
                        flat_prov
                    );
                }
            }
            match flat_prov {
                Provenance::Global { resource_type, address_source } => {
                    // TEMP DEBUG EXPERIMENT
                    if resource_type.contains("ManagedFungibleAsset") {
                        eprintln!(
                            "[SKIP-MFA-WRITE] {}",
                            resource_type
                        );
                        continue;
                    }
                    self.summary.write_set.insert(AccessPath {
                        resource_type,
                        address_source: *address_source,
                    });
                },
                Provenance::Param(idx) => {
                    self.summary.param_side_effects.insert(idx, SideEffect::Write);
                },
                _ => {}
            }
        }
    }

    fn apply_callee_summary(&mut self, callee_summary: &FunctionSummary, args: &Vec<AbstractValue>) {
        for path in &callee_summary.read_set {
            for hydrated in self.hydrate_access_paths(path, args) {
                self.summary.read_set.insert(hydrated);
            }
        }
        for path in &callee_summary.write_set {
            eprintln!(
                "[SUMMARY-IN ] {:?}",
                path
            );
            for hydrated in self.hydrate_access_paths(path, args) {
                eprintln!(
                    "[SUMMARY-OUT] {:?}",
                    hydrated
                );
                self.summary.write_set.insert(hydrated);
            }
        }
        for (param_idx, effect) in &callee_summary.param_side_effects {
            if *effect == SideEffect::Write {
                if let Some(arg) = args.get(*param_idx) {
                    eprintln!(
                        "[SIDE-EFFECT] param={} arg={:?}",
                        param_idx,
                        arg.provenance
                    );
                    self.handle_write_to_provenance(&arg.provenance);
                }
            }
        }
    }

    /// Substitutes symbolic `Param(i)` references in an `AccessPath` with the actual argument
    /// provenances, then flattens. Returns one `AccessPath` per root pointer found — a
    /// multi-field struct argument expands into multiple paths instead of silently losing all
    /// but the first.
    fn hydrate_access_paths(&self, path: &AccessPath, args: &Vec<AbstractValue>) -> Vec<AccessPath> {
        let hydrated_source = self.hydrate_provenance(&path.address_source, args);
        hydrated_source.flatten()
            .into_iter()
            .map(|flat_source| AccessPath {
                resource_type: path.resource_type.clone(),
                address_source: flat_source,
            })
            .collect()
    }

    fn hydrate_provenance(&self, prov: &Provenance, args: &Vec<AbstractValue>) -> Provenance {
        
        if format!("{:?}", prov).contains("ManagedFungibleAsset") {
            eprintln!("[HYDRATE-IN ] {:?}", prov);
        }

        let result = match prov {
            Provenance::Param(idx) => {
                if let Some(arg) = args.get(*idx) {
                    arg.provenance.clone()
                } else {
                    Provenance::Bottom
                }
            },
            Provenance::Global { resource_type, address_source } => {
                Provenance::Global {
                    resource_type: resource_type.clone(),
                    address_source: Box::new(self.hydrate_provenance(address_source, args)),
                }
            },
            Provenance::Field(parent) => {
                Provenance::Field(Box::new(self.hydrate_provenance(parent, args)))
            },
            Provenance::Struct(fields) => {
                let hydrated_fields = fields.iter().map(|f| self.hydrate_provenance(f, args)).collect();
                Provenance::Struct(hydrated_fields)
            },
            _ => prov.clone(),
        };
        if format!("{:?}", result).contains("ManagedFungibleAsset") {
            eprintln!("[HYDRATE-OUT] {:?}", result);
        }
        result
    }

    // --- LOOKUP HELPERS ---

    fn get_field_count_def(&self, idx: move_binary_format::file_format::StructDefinitionIndex) -> usize {
        let struct_def = self.module.struct_def_at(idx);
        match &struct_def.field_information {
            move_binary_format::file_format::StructFieldInformation::Declared(fields) => fields.len(),
            _ => 0,
        }
    }

    fn get_field_count_inst(&self, idx: move_binary_format::file_format::StructDefInstantiationIndex) -> usize {
        let inst = self.module.struct_instantiation_at(idx);
        let struct_def = self.module.struct_def_at(inst.def);
        match &struct_def.field_information {
            move_binary_format::file_format::StructFieldInformation::Declared(fields) => fields.len(),
            _ => 0,
        }
    }

    fn resolve_struct_name_from_def(&self, idx: move_binary_format::file_format::StructDefinitionIndex) -> String {
        let struct_def = self.module.struct_def_at(idx);
        let struct_handle = self.module.struct_handle_at(struct_def.struct_handle);
        self.resolve_struct_handle_name(struct_handle)
    }

    fn resolve_struct_handle_name(&self, handle: &move_binary_format::file_format::StructHandle) -> String {
        let module_handle = self.module.module_handle_at(handle.module);
        let module_id = self.module.module_id_for_handle(module_handle);
        let struct_name = self.module.identifier_at(handle.name);
        format!("{}::{}::{}", module_id.address().to_hex_literal(), module_id.name(), struct_name)
    }

    // fn resolve_field_type(&self, idx: move_binary_format::file_format::FieldHandleIndex) -> String {
    //     let field_handle = self.module.field_handle_at(idx);
    //     let struct_def = self.module.struct_def_at(field_handle.owner);
    //     match &struct_def.field_information {
    //         move_binary_format::file_format::StructFieldInformation::Declared(fields) => {
    //             if let Some(field_def) = fields.get(field_handle.field as usize) {
    //                 return self.module.identifier_at(field_def.name).to_string();
    //             }
    //         },
    //         _ => {}
    //     }
    //     "UnknownField".to_string()
    // }
}