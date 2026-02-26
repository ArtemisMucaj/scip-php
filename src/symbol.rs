use scip::types::descriptor::Suffix;
use scip::types::{Descriptor, Package, Symbol};

const SCHEME: &str = "scip-php";
const MANAGER: &str = "composer";

/// Package context for symbol construction.
#[derive(Debug, Clone)]
pub struct PhpPackage {
    pub name: String,
    pub version: String,
}

impl PhpPackage {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        PhpPackage {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Create a placeholder package for files not belonging to any composer package.
    pub fn local() -> Self {
        PhpPackage {
            name: ".".to_string(),
            version: ".".to_string(),
        }
    }

    fn to_scip_package(&self) -> Package {
        Package {
            manager: MANAGER.to_string(),
            name: self.name.clone(),
            version: self.version.clone(),
            ..Default::default()
        }
    }
}

/// Builds SCIP symbol strings for PHP entities.
pub struct SymbolBuilder<'a> {
    package: &'a PhpPackage,
}

impl<'a> SymbolBuilder<'a> {
    pub fn new(package: &'a PhpPackage) -> Self {
        SymbolBuilder { package }
    }

    /// Create a SCIP Symbol from descriptors.
    fn make_symbol(&self, descriptors: Vec<Descriptor>) -> Symbol {
        Symbol {
            scheme: SCHEME.to_string(),
            package: protobuf::MessageField::some(self.package.to_scip_package()),
            descriptors,
            ..Default::default()
        }
    }

    fn namespace_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Namespace.into(),
            ..Default::default()
        }
    }

    fn type_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Type.into(),
            ..Default::default()
        }
    }

    fn term_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Term.into(),
            ..Default::default()
        }
    }

    fn method_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Method.into(),
            ..Default::default()
        }
    }

    fn parameter_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::Parameter.into(),
            ..Default::default()
        }
    }

    fn type_parameter_descriptor(name: &str) -> Descriptor {
        Descriptor {
            name: name.to_string(),
            suffix: Suffix::TypeParameter.into(),
            ..Default::default()
        }
    }

    /// Split a fully-qualified PHP name like "App\Models\User" into
    /// namespace parts and a final name.
    fn split_fqn(fqn: &str) -> (Vec<&str>, &str) {
        let fqn = fqn.strip_prefix('\\').unwrap_or(fqn);
        let parts: Vec<&str> = fqn.split('\\').collect();
        if parts.len() <= 1 {
            (vec![], parts.first().copied().unwrap_or(""))
        } else {
            let (ns, name) = parts.split_at(parts.len() - 1);
            (ns.to_vec(), name[0])
        }
    }

    fn namespace_descriptors(ns_parts: &[&str]) -> Vec<Descriptor> {
        ns_parts
            .iter()
            .map(|part| Self::namespace_descriptor(part))
            .collect()
    }

    /// Create a symbol for a namespace (e.g., "App\Models").
    pub fn namespace_symbol(&self, fqn: &str) -> Symbol {
        let fqn = fqn.strip_prefix('\\').unwrap_or(fqn);
        let parts: Vec<&str> = fqn.split('\\').collect();
        let descriptors = parts
            .iter()
            .map(|part| Self::namespace_descriptor(part))
            .collect();
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a class-like (class, interface, trait, enum).
    pub fn class_like_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a function.
    pub fn function_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::method_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a method on a class-like.
    pub fn method_symbol(&self, class_fqn: &str, method: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::method_descriptor(method));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a property on a class-like.
    /// `property` should not include the `$` prefix.
    pub fn property_symbol(&self, class_fqn: &str, property: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::term_descriptor(property));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a class constant.
    pub fn class_constant_symbol(&self, class_fqn: &str, constant: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::term_descriptor(constant));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for an enum case.
    pub fn enum_case_symbol(&self, enum_fqn: &str, case_name: &str) -> Symbol {
        let (ns_parts, enum_name) = Self::split_fqn(enum_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(enum_name));
        descriptors.push(Self::term_descriptor(case_name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a top-level constant.
    pub fn constant_symbol(&self, fqn: &str) -> Symbol {
        let (ns_parts, name) = Self::split_fqn(fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::term_descriptor(name));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a method parameter.
    pub fn parameter_symbol(&self, class_fqn: &str, method: &str, param: &str) -> Symbol {
        let (ns_parts, class_name) = Self::split_fqn(class_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::type_descriptor(class_name));
        descriptors.push(Self::method_descriptor(method));
        descriptors.push(Self::parameter_descriptor(param));
        self.make_symbol(descriptors)
    }

    /// Create a symbol for a function parameter.
    pub fn function_parameter_symbol(&self, func_fqn: &str, param: &str) -> Symbol {
        let (ns_parts, func_name) = Self::split_fqn(func_fqn);
        let mut descriptors = Self::namespace_descriptors(&ns_parts);
        descriptors.push(Self::method_descriptor(func_name));
        descriptors.push(Self::parameter_descriptor(param));
        self.make_symbol(descriptors)
    }

    /// Create a local symbol (file-scoped, for local variables).
    pub fn local_symbol(id: usize) -> String {
        format!("local {}", id)
    }
}

/// Format a SCIP Symbol struct into its string representation.
pub fn format_symbol(symbol: &Symbol) -> String {
    scip::symbol::format_symbol(symbol.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_package() -> PhpPackage {
        PhpPackage::new("vendor/myapp", "1.0.0")
    }

    #[test]
    fn test_class_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.class_like_symbol("App\\Models\\User");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/User#");
    }

    #[test]
    fn test_method_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.method_symbol("App\\Models\\User", "getName");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#getName()."
        );
    }

    #[test]
    fn test_property_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.property_symbol("App\\Models\\User", "name");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#name."
        );
    }

    #[test]
    fn test_function_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.function_symbol("App\\Utils\\formatDate");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Utils/formatDate()."
        );
    }

    #[test]
    fn test_global_function_no_namespace() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.function_symbol("array_map");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 array_map().");
    }

    #[test]
    fn test_constant_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.constant_symbol("App\\Config\\VERSION");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Config/VERSION."
        );
    }

    #[test]
    fn test_enum_case_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.enum_case_symbol("App\\Enums\\Status", "Active");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Enums/Status#Active."
        );
    }

    #[test]
    fn test_parameter_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.parameter_symbol("App\\Models\\User", "setName", "name");
        let s = format_symbol(&sym);
        assert_eq!(
            s,
            "scip-php composer vendor/myapp 1.0.0 App/Models/User#setName().(name)"
        );
    }

    #[test]
    fn test_local_symbol() {
        assert_eq!(SymbolBuilder::local_symbol(0), "local 0");
        assert_eq!(SymbolBuilder::local_symbol(42), "local 42");
    }

    #[test]
    fn test_leading_backslash_stripped() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.class_like_symbol("\\App\\Models\\User");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/User#");
    }

    #[test]
    fn test_namespace_symbol() {
        let pkg = test_package();
        let builder = SymbolBuilder::new(&pkg);
        let sym = builder.namespace_symbol("App\\Models");
        let s = format_symbol(&sym);
        assert_eq!(s, "scip-php composer vendor/myapp 1.0.0 App/Models/");
    }
}
