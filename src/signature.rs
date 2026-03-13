use mago_syntax::ast::function_like::parameter::{FunctionLikeParameter, FunctionLikeParameterList};
use mago_syntax::ast::modifier::Modifier;
use mago_syntax::ast::type_hint::Hint;

/// Format a type hint as a PHP source string.
pub fn hint_to_string(hint: &Hint<'_>) -> String {
    use mago_syntax::ast::type_hint::Hint::*;
    match hint {
        Identifier(id) => id.value().to_string(),
        Nullable(n) => format!("?{}", hint_to_string(&n.hint)),
        Union(u) => format!("{}|{}", hint_to_string(u.left), hint_to_string(u.right)),
        Intersection(i) => format!("{}&{}", hint_to_string(i.left), hint_to_string(i.right)),
        Parenthesized(p) => format!("({})", hint_to_string(p.hint)),
        Null(_) => "null".to_string(),
        True(_) => "true".to_string(),
        False(_) => "false".to_string(),
        Array(_) => "array".to_string(),
        Callable(_) => "callable".to_string(),
        Static(_) => "static".to_string(),
        Self_(_) => "self".to_string(),
        Parent(_) => "parent".to_string(),
        Void(id) | Never(id) | Float(id) | Bool(id) | Integer(id) | String(id) | Object(id)
        | Mixed(id) | Iterable(id) => id.value.to_string(),
    }
}

/// Format a single modifier as its PHP keyword.
pub fn modifier_to_str(modifier: &Modifier<'_>) -> &'static str {
    match modifier {
        Modifier::Public(_) => "public",
        Modifier::Protected(_) => "protected",
        Modifier::Private(_) => "private",
        Modifier::PublicSet(_) => "public(set)",
        Modifier::ProtectedSet(_) => "protected(set)",
        Modifier::PrivateSet(_) => "private(set)",
        Modifier::Static(_) => "static",
        Modifier::Abstract(_) => "abstract",
        Modifier::Final(_) => "final",
        Modifier::Readonly(_) => "readonly",
    }
}

/// Format a sequence of modifiers as a space-separated string.
pub fn modifiers_to_string<'a>(modifiers: impl Iterator<Item = &'a Modifier<'a>>) -> String {
    modifiers
        .map(modifier_to_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format a single function parameter as a PHP source string.
/// Includes promoted property visibility, type hint, reference/variadic markers and name.
/// Default values are omitted for brevity.
pub fn param_to_string(param: &FunctionLikeParameter<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Promoted property modifiers (e.g. `public`, `readonly`)
    for modifier in param.modifiers.iter() {
        parts.push(modifier_to_str(modifier).to_string());
    }

    // Type hint
    if let Some(hint) = &param.hint {
        parts.push(hint_to_string(hint));
    }

    // Reference + variadic + name
    let mut var_part = String::new();
    if param.ampersand.is_some() {
        var_part.push('&');
    }
    if param.ellipsis.is_some() {
        var_part.push_str("...");
    }
    // param.variable.name already includes the leading `$`
    var_part.push_str(param.variable.name);

    parts.push(var_part);
    parts.join(" ")
}

/// Format the parameter list as `(type $name, ...)`.
pub fn params_to_string(list: &FunctionLikeParameterList<'_>) -> String {
    let params: Vec<String> = list.parameters.iter().map(param_to_string).collect();
    format!("({})", params.join(", "))
}

/// Build the ` ```php\n{sig}\n``` ` documentation entry.
pub fn signature_doc_block(sig: &str) -> String {
    format!("```php\n{}\n```", sig)
}

/// Prepend the signature code block to an existing PHPDoc documentation vec.
pub fn with_signature(sig: String, phpdoc: Vec<String>) -> Vec<String> {
    let mut docs = Vec::with_capacity(1 + phpdoc.len());
    docs.push(signature_doc_block(&sig));
    docs.extend(phpdoc);
    docs
}
