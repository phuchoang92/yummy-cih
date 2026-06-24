//! JavaConstantResolver — resolves Java `static final String` constant names to
//! their folded literal values. Handles simple identifiers, qualified names
//! (`Cls.NAME`), static imports, and one-level inherited constants.

use std::collections::HashMap;

use cih_core::StringConstant;

use crate::constant_resolver::{ConstantResolver, ResolutionContext};

/// Index key: `(owner_fqcn, const_name)` → folded value.
type ConstantIndex = HashMap<(String, String), String>;

/// Index of static imports: `(file_path, imported_name)` → owner_fqcn.
type StaticImportIndex = HashMap<(String, String), String>;

/// Index of superclass chains: `owner_fqcn` → parent_fqcn (one level).
type SuperIndex = HashMap<String, String>;

pub struct JavaConstantResolver {
    /// (owner_fqcn, const_name) → value
    index: ConstantIndex,
    /// For static imports: map from (owner_fqcn, member_name) → const_name → value.
    /// We reuse `index` for this; static imports just resolve owner_fqcn from imports.
    /// simple_name → owner_fqcn (for single-class imports `import static pkg.Cls.NAME`)
    #[allow(dead_code)]
    static_import_owners: StaticImportIndex,
    /// owner_fqcn → parent_fqcn (one level, for inheritance)
    super_index: SuperIndex,
    /// type simple_name → fqcn (from imports)
    type_index: HashMap<String, String>,
}

impl JavaConstantResolver {
    pub fn build(constants: &[StringConstant], all_defs: &[(String, Option<String>)]) -> Self {
        let mut index = ConstantIndex::new();
        let mut type_index: HashMap<String, String> = HashMap::new();
        let mut super_index: SuperIndex = HashMap::new();

        for c in constants {
            if !c.dynamic {
                index.insert((c.owner_fqcn.clone(), c.const_name.clone()), c.value.clone());
            }
        }

        // Build type index from all type defs (fqcn, simple_name pairs)
        for (fqcn, _super_fqcn) in all_defs {
            let simple = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str()).to_string();
            type_index.entry(simple).or_insert_with(|| fqcn.clone());
        }
        for (fqcn, super_fqcn) in all_defs {
            if let Some(sup) = super_fqcn {
                super_index.insert(fqcn.clone(), sup.clone());
            }
        }

        Self {
            index,
            static_import_owners: StaticImportIndex::new(),
            super_index,
            type_index,
        }
    }
}

impl ConstantResolver for JavaConstantResolver {
    fn resolve(&self, name: &str, ctx: &ResolutionContext<'_>) -> Option<String> {

        // 1. Simple identifier in same class
        if !name.contains('.') {
            // Try owner class first
            if let Some(v) = self.index.get(&(ctx.owner_fqcn.to_string(), name.to_string())) {
                return Some(v.clone());
            }
            // Try static imports
            for imp in ctx.imports {
                if !imp.is_static {
                    continue;
                }
                if imp.is_wildcard {
                    // `import static com.example.Config.*`
                    let owner = imp.raw.trim_end_matches(".*");
                    if let Some(v) = self.index.get(&(owner.to_string(), name.to_string())) {
                        return Some(v.clone());
                    }
                } else if let Some((owner, imported_name)) = imp.raw.rsplit_once('.') {
                    if imported_name == name {
                        if let Some(v) = self.index.get(&(owner.to_string(), name.to_string())) {
                            return Some(v.clone());
                        }
                    }
                }
            }
            // Try inherited (one level)
            if let Some(parent_fqcn) = self.super_index.get(ctx.owner_fqcn) {
                if let Some(v) = self.index.get(&(parent_fqcn.clone(), name.to_string())) {
                    return Some(v.clone());
                }
            }
            return None;
        }

        // 2. Qualified `Cls.NAME`
        if let Some((cls_part, const_name)) = name.rsplit_once('.') {
            // Try to resolve cls_part to an fqcn via imports
            let resolved_fqcn = self.resolve_type(cls_part, ctx);
            if let Some(fqcn) = resolved_fqcn {
                if let Some(v) = self.index.get(&(fqcn, const_name.to_string())) {
                    return Some(v.clone());
                }
            }
            // Also try cls_part directly as fqcn
            if let Some(v) = self.index.get(&(cls_part.to_string(), const_name.to_string())) {
                return Some(v.clone());
            }
        }

        None
    }
}

impl JavaConstantResolver {
    fn resolve_type(&self, simple_or_qualified: &str, ctx: &ResolutionContext<'_>) -> Option<String> {
        // Already qualified?
        if simple_or_qualified.contains('.') {
            return Some(simple_or_qualified.to_string());
        }
        // Check explicit imports
        for imp in ctx.imports {
            if imp.is_static || imp.is_wildcard {
                continue;
            }
            if let Some((_, imported_name)) = imp.raw.rsplit_once('.') {
                if imported_name == simple_or_qualified {
                    return Some(imp.raw.clone());
                }
            }
        }
        // Check type index
        self.type_index.get(simple_or_qualified).cloned()
    }
}
