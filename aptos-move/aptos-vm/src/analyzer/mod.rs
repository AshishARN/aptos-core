use std::collections::{HashMap, HashSet};
use dashmap::DashMap;
use move_binary_format::CompiledModule;
use move_binary_format::file_format::{Bytecode, FunctionHandleIndex};
use move_binary_format::access::ModuleAccess; 
use move_core_types::account_address::AccountAddress;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::ModuleId;
use std::str::FromStr;

pub mod abstract_domain;
pub mod interpreter;
use abstract_domain::*;
use interpreter::Interpreter;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FunctionID {
    pub address: String,
    pub module: String,
    pub name: String,
}

impl FunctionID {
    pub fn to_string(&self) -> String {
        format!("{}::{}::{}", self.address, self.module, self.name)
    }
}

// ---------------------------------------------------------
// JIT ENTRY POINT: Replaces the old `main()`
// ---------------------------------------------------------
pub fn analyze_function_with_dependencies(
    root_func_id: &FunctionID,
    loaded_modules: &HashMap<ModuleId, CompiledModule>,
    global_summaries: &DashMap<FunctionID, FunctionSummary>
) {
    // 1. Build the Global Call Graph from the modules fetched from the DB
    let mut call_graph: HashMap<FunctionID, HashSet<FunctionID>> = HashMap::new();

    for (module_id, module) in loaded_modules {
        for func_def in &module.function_defs {
            let caller_handle = module.function_handle_at(func_def.function);
            let caller_name = module.identifier_at(caller_handle.name).to_string();
            let caller_id = FunctionID {
                address: module_id.address().to_hex_literal(),
                module: module_id.name().to_string(),
                name: caller_name,
            };

            if let Some(code_unit) = &func_def.code {
                for instruction in &code_unit.code {
                    let handle_index_opt = match instruction {
                        Bytecode::Call(idx) => Some(*idx),
                        Bytecode::CallGeneric(idx) => {
                            let instantiation = module.function_instantiation_at(*idx);
                            Some(instantiation.handle)
                        }
                        _ => None,
                    };

                    if let Some(callee_handle_idx) = handle_index_opt {
                        add_dependency(module, callee_handle_idx, &caller_id, &mut call_graph);
                    }
                }
            }
        }
    }

    // eprintln!("[MODMAP] loaded_modules keys: {:?}", 
    //     loaded_modules.keys().map(|k| format!("{}::{}", k.address(), k.name())).collect::<Vec<_>>()
    // );
    eprintln!("[MODMAP] keys: {:?}", 
        loaded_modules.keys().map(|k| k.name().to_string()).collect::<Vec<_>>());
    // eprintln!("[MODMAP] root_func_id address='{}' module='{}'", 
    //     root_func_id.address, root_func_id.module
    // );

    // 2. Get Execution Order (Bottom-Up DFS)
    let execution_order = get_execution_order(root_func_id, &call_graph);

    // 3. Analyze everything in the chain sequentially
    for func_id in execution_order {
        if global_summaries.contains_key(&func_id) {
            continue; // Memoization
        }

        let owner_module_opt = find_module_by_id(&func_id, loaded_modules);

        if let Some(module) = owner_module_opt {
            if let Some(func_def) = find_function_def(module, &func_id.name) {
                let mut interpreter = Interpreter::new(module, global_summaries);
                let summary = interpreter.analyze_function(func_def);

                for path in &summary.write_set {
                    let s = format!("{:?}", path);

                    if s.contains("ManagedFungibleAsset") {
                        eprintln!(
                            "[SUMMARY-CREATED] {}::{}::{} => {} @ {:?}",
                            func_id.address,
                            func_id.module,
                            func_id.name,
                            path.resource_type,
                            path.address_source
                        );
                    }
                }

                global_summaries.insert(func_id.clone(), summary);
            } else {
                global_summaries.insert(func_id.clone(), FunctionSummary::new());
            }
        } else {
            global_summaries.insert(func_id.clone(), FunctionSummary::new());
        }
    }

    // ADD THIS — print the final summary for the root function:
    if let Some(summary) = global_summaries.get(root_func_id) {
        eprintln!("[SUMMARY] {}::{}::{} reads={} writes={} params={:?}",
            root_func_id.address, root_func_id.module, root_func_id.name,
            summary.read_set.len(), summary.write_set.len(),
            summary.param_side_effects.keys().collect::<Vec<_>>()
        );
        for path in &summary.read_set {
            eprintln!("[SUMMARY-READ]  {} @ {:?}", path.resource_type, path.address_source);
        }
        for path in &summary.write_set {
            eprintln!("[SUMMARY-WRITE] {} @ {:?}", path.resource_type, path.address_source);
        }
    }
}

// ---------------------------------------------------------
// RESTORED HELPER FUNCTIONS
// ---------------------------------------------------------

pub fn find_module_by_id<'a>(
    func_id: &FunctionID, 
    modules: &'a HashMap<ModuleId, CompiledModule>
) -> Option<&'a CompiledModule> {
    let addr_res = AccountAddress::from_hex_literal(&func_id.address);
    let addr = match addr_res {
        Ok(a) => a,
        Err(_) => AccountAddress::from_str_strict(&func_id.address).unwrap_or(AccountAddress::ONE)
    };
    let ident = Identifier::new(func_id.module.as_str()).ok()?;
    let mod_id = ModuleId::new(addr, ident);
    modules.get(&mod_id)
}

pub fn find_function_def<'a>(
    module: &'a CompiledModule, 
    func_name: &str
) -> Option<&'a move_binary_format::file_format::FunctionDefinition> {
    module.function_defs.iter().find(|def| {
        let handle = module.function_handle_at(def.function);
        let name_ident = module.identifier_at(handle.name);
        name_ident.as_str() == func_name
    })
}

fn get_execution_order(
    root: &FunctionID,
    graph: &HashMap<FunctionID, HashSet<FunctionID>>
) -> Vec<FunctionID> {
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    dfs(root, graph, &mut visited, &mut result);
    result
}

fn dfs(
    current: &FunctionID,
    graph: &HashMap<FunctionID, HashSet<FunctionID>>,
    visited: &mut HashSet<FunctionID>,
    result: &mut Vec<FunctionID>
) {
    if visited.contains(current) { return; }
    visited.insert(current.clone());

    if let Some(neighbors) = graph.get(current) {
        let mut sorted_neighbors: Vec<&FunctionID> = neighbors.iter().collect();
        sorted_neighbors.sort_by(|a, b| a.name.cmp(&b.name));
        for neighbor in sorted_neighbors {
            dfs(neighbor, graph, visited, result);
        }
    }
    result.push(current.clone());
}

fn add_dependency(
    module: &CompiledModule,
    handle_idx: FunctionHandleIndex,
    caller_id: &FunctionID,
    graph: &mut HashMap<FunctionID, HashSet<FunctionID>>
) {
    let callee_handle = module.function_handle_at(handle_idx);
    let callee_mod_handle = module.module_handle_at(callee_handle.module);
    let callee_mod_id = module.module_id_for_handle(callee_mod_handle);
    let callee_name = module.identifier_at(callee_handle.name).to_string();
    
    let callee_id = FunctionID {
        address: callee_mod_id.address().to_hex_literal(),
        module: callee_mod_id.name().to_string(),
        name: callee_name,
    };

    graph.entry(caller_id.clone()).or_insert_with(HashSet::new).insert(callee_id);
}