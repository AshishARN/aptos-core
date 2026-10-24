use std::collections::{HashSet, HashMap};

// 1. PROVENANCE (π)
// Where did this value come from?
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Provenance {
    // The value came from the i-th argument of the function (e.g., Param(0))
    Param(usize),

    // The value points to a Global Resource T at a specific address (Source)
    // Global(TypeString, AddressSource)
    Global {
        resource_type: String, // e.g., "0x1::coin::CoinStore"
        address_source: Box<Provenance>, // Recursive: Where did the address come from?
        // In rust, an enum variant cannot directly contain itself, so we use Box to create a heap-allocated pointer to another Provenance
    },

    // The value is a hardcoded constant (e.g., @0x1)
    Const(String),

    Field(Box<Provenance>),

    // A local variable (temporary register) - useful for intermediate steps
    Local(usize),

    TypeArg(usize), // Tracks <T0>, <T1>, etc.

    Struct(Vec<Provenance>), 
    
    // We don't know/don't care (e.g., result of 1 + 2)
    Bottom,
}

// 2. ABSTRACT VALUE (v#)
// The tuple <Type, Provenance> on the stack
#[derive(Debug, Clone, PartialEq)]
pub struct AbstractValue {
    pub provenance: Provenance, // π (Origin)
}

// 3. ACCESS PATH (For Results)
// A simplified view for the Read/Write sets: <Resource, AddressSource>
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccessPath {
    pub resource_type: String,
    pub address_source: Provenance,
}

// 4. FUNCTION SUMMARY (Σf)
// The final output of analyzing a function
#[derive(Debug, Clone)]
pub struct FunctionSummary {
    // R: What global resources does this function read?
    pub read_set: HashSet<AccessPath>,

    // W: What global resources does this function write?
    pub write_set: HashSet<AccessPath>,

    // E_param: Does this function modify its own arguments?
    // Map: ArgIndex -> "Write"
    pub param_side_effects: HashMap<usize, SideEffect>,

    // Maps return value index (0, 1...) to its Provenance
    // Example: { 0: Param(1) } means "Return value #0 comes from Argument #1"
    pub return_dependencies: HashMap<usize, Provenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    Write, // The argument is a mutable reference that gets written to
}

impl FunctionSummary {
    pub fn new() -> Self {
        Self {
            read_set: HashSet::new(),
            write_set: HashSet::new(),
            param_side_effects: HashMap::new(),
            return_dependencies: HashMap::new(),
        }
    }
}

impl AbstractValue {
    pub fn new_bottom() -> Self {
        Self {
            provenance: Provenance::Bottom,
        }
    }
}

impl Provenance {
    /// Strips away in-memory layout wrappers (Structs and Fields) to expose all root pointers.
    ///
    /// Returns a `Vec` because a `Struct` can carry multiple fields with distinct provenances
    /// (e.g. `[Param(0), Param(1)]`). Returning only the first root — the old behaviour — would
    /// silently drop every subsequent one, causing missed read/write set entries for callers that
    /// pass multi-field structs as addresses.
    ///
    /// Callers must iterate the returned vec and record one `AccessPath` per entry. An empty vec
    /// is equivalent to the old `Bottom` result (no storage implications).
    pub fn flatten(&self) -> Vec<Provenance> {
        match self {
            // A Struct is an in-memory aggregate — recurse into every field and collect all roots.
            Provenance::Struct(fields) => {
                fields.iter()
                    .flat_map(|f| f.flatten())
                    .collect()
            },

            // A Field borrow — we care only about what the *parent* ultimately points at.
            Provenance::Field(parent) => parent.flatten(),

            // A Global keeps its shape but its inner address source must itself be flattened.
            // Because the address source of a Global is always a single pointer (not a struct),
            // flatten() on it returns at most one element; we take the first or fall back to Bottom.
            Provenance::Global { resource_type, address_source } => {
                let flat_sources = address_source.flatten();
                let resolved_source = flat_sources.into_iter().next().unwrap_or(Provenance::Bottom);
                vec![Provenance::Global {
                    resource_type: resource_type.clone(),
                    address_source: Box::new(resolved_source),
                }]
            },

            // Bottom carries no storage implication — return an empty vec so callers skip it.
            Provenance::Bottom => vec![Provenance::Bottom],

            // Param, TypeArg, Const — already atomic roots, wrap in a single-element vec.
            _ => vec![self.clone()],
        }
    }
}