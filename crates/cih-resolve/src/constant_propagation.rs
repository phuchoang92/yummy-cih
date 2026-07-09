//! Gap 4: Build a Java ConstantResolver from parsed file data.
//!
//! The resolver is used during the emit phase (Gap 3) to fold static-final
//! String constants into call-site argument texts.

use cih_core::ParsedFile;
use cih_lang::constant_resolver::ConstantResolver;
use cih_lang::java::constant_resolver::JavaConstantResolver;

/// Build a `JavaConstantResolver` from the full set of parsed files.
///
/// Collects all `string_constants` from every file and the full (fqcn, super_fqcn) list
/// from all type defs, then delegates to `JavaConstantResolver::build`.
pub fn build_java_constant_resolver(parsed: &[ParsedFile]) -> impl ConstantResolver {
    let mut constants = Vec::new();
    let mut all_defs: Vec<(String, Option<String>)> = Vec::new();

    for pf in parsed {
        constants.extend(pf.string_constants.iter().cloned());
        // Collect (fqcn, super_fqcn) for all type-level defs
        for def in &pf.defs {
            use cih_core::NodeKind;
            if matches!(
                def.kind,
                NodeKind::Class | NodeKind::Interface | NodeKind::Enum
            ) {
                // The fqcn for a type def is the same as its qualified_name; we use def.fqcn.
                // super_fqcn is not tracked in SymbolDef, so pass None here —
                // the resolver will still handle simple / qualified / static-import lookups.
                all_defs.push((def.fqcn.clone(), None));
            }
        }
    }

    JavaConstantResolver::build(&constants, &all_defs)
}
