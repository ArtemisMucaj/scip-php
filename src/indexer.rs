use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, Result};
use mago_span::HasSpan;
use protobuf::MessageField;
use scip::types::{
    self, Document, Index, Metadata, Occurrence, PositionEncoding, SymbolInformation, ToolInfo,
};

use crate::line_index::LineIndex;
use crate::project::PhpProject;
use crate::symbol::{format_symbol, SymbolBuilder};

/// SCIP indexer for PHP projects.
pub struct Indexer {
    project: PhpProject,
    /// Maps variable names (e.g. "$user") to their resolved class FQN (e.g. "App\\Models\\User").
    /// Populated from parameter type hints before walking method/function bodies, cleared after.
    var_types: RefCell<HashMap<String, String>>,
    /// Maps class FQN → (property name → property type FQN).
    ///
    /// Built in a lightweight pre-pass over all project files before the main indexing pass.
    /// This enables type resolution for chained property accesses:
    ///
    ///   `$device->home->advertiseUsersAboutReset(...)` resolves to
    ///   `Netatmo/Models/Homes/Home#advertiseUsersAboutReset()` because we know
    ///   `Device::$home` has type `Home`.
    property_types: RefCell<HashMap<String, HashMap<String, String>>>,
    /// Maps class FQN → (method name → return type FQN).
    ///
    /// Built alongside `property_types` in the pre-pass.  This allows
    /// `try_resolve_class_from_expr` to propagate types through method calls:
    ///
    ///   `self::loadDevice()` is typed as `Device` if `loadDevice()` has a
    ///   native return type hint or a PHPDoc `@return Device` tag.
    method_return_types: RefCell<HashMap<String, HashMap<String, String>>>,
}

impl Indexer {
    pub fn new(project: PhpProject) -> Self {
        Indexer {
            project,
            var_types: RefCell::new(HashMap::new()),
            property_types: RefCell::new(HashMap::new()),
            method_return_types: RefCell::new(HashMap::new()),
        }
    }

    /// Run the indexer and produce a SCIP Index.
    pub fn index(&self) -> Result<Index> {
        let php_files = self.project.discover_php_files();
        eprintln!("scip-php: found {} PHP files", php_files.len());

        // Pre-pass: build the global property-type map so that chained property accesses
        // (e.g. `$device->home->method()`) can be resolved during the main indexing pass.
        for file_path in &php_files {
            self.collect_property_types_from_file(file_path);
        }
        eprintln!(
            "scip-php: collected property types for {} classes, method return types for {} methods",
            self.property_types.borrow().len(),
            self.method_return_types
                .borrow()
                .values()
                .map(|m| m.len())
                .sum::<usize>(),
        );

        let mut documents = Vec::new();

        for file_path in &php_files {
            match self.index_file(file_path) {
                Ok(Some(doc)) => documents.push(doc),
                Ok(None) => {} // Skip files with no indexable content
                Err(e) => {
                    eprintln!(
                        "scip-php: warning: failed to index {}: {}",
                        file_path.display(),
                        e
                    );
                }
            }
        }

        eprintln!("scip-php: indexed {} documents", documents.len());

        let project_root = format!("file://{}", self.project.root.to_string_lossy());

        let index = Index {
            metadata: MessageField::some(Metadata {
                version: types::ProtocolVersion::UnspecifiedProtocolVersion.into(),
                tool_info: MessageField::some(ToolInfo {
                    name: "scip-php".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    ..Default::default()
                }),
                project_root,
                text_document_encoding: types::TextEncoding::UTF8.into(),
                ..Default::default()
            }),
            documents,
            ..Default::default()
        };

        Ok(index)
    }

    /// Index a single PHP file and produce a SCIP Document.
    fn index_file(&self, file_path: &std::path::Path) -> Result<Option<Document>> {
        let source = std::fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read {}", file_path.display()))?;

        let relative_path = self.project.relative_path(file_path);
        let line_index = LineIndex::new(&source);

        // Parse the file using Mago
        let arena = bumpalo::Bump::new();
        let file = mago_database::file::File::ephemeral(
            relative_path.clone().into(),
            source.clone().into(),
        );
        let program = mago_syntax::parser::parse_file(&arena, &file);

        // Check for parse errors -- still index what we can
        if !program.errors.is_empty() {
            eprintln!(
                "scip-php: {} parse error(s) in {}",
                program.errors.len(),
                relative_path
            );
        }

        // Resolve names (handles use statements, namespace resolution)
        let resolver = mago_names::resolver::NameResolver::new(&arena);
        let resolved_names = resolver.resolve(program);

        // Build SCIP data by walking the AST
        let builder = SymbolBuilder::new(&self.project.package);
        let mut occurrences = Vec::new();
        let mut symbols = Vec::new();
        let mut local_counter: usize = 0;

        // Walk statements to find definitions
        self.walk_statements(
            &program.statements,
            &program.trivia,
            &resolved_names,
            &builder,
            &line_index,
            program.source_text,
            &mut occurrences,
            &mut symbols,
            &mut local_counter,
            None,
        );

        if occurrences.is_empty() && symbols.is_empty() {
            return Ok(None);
        }

        Ok(Some(Document {
            relative_path,
            language: "PHP".to_string(),
            occurrences,
            symbols,
            position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
            ..Default::default()
        }))
    }

    /// Walk a sequence of statements and emit occurrences.
    fn walk_statements<'arena>(
        &self,
        statements: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Statement<'arena>>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
        enclosing_class_fqn: Option<&str>,
    ) {
        use mago_syntax::ast::Statement;

        for stmt in statements.iter() {
            match stmt {
                Statement::Namespace(ns) => {
                    self.walk_namespace(
                        ns,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Function(func) => {
                    self.walk_function(
                        func,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Class(class) => {
                    self.walk_class(
                        class,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Interface(iface) => {
                    self.walk_interface(
                        iface,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Trait(trait_def) => {
                    self.walk_trait(
                        trait_def,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Enum(enum_def) => {
                    self.walk_enum(
                        enum_def,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Constant(constant) => {
                    self.walk_constant(
                        constant,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                    );
                }
                Statement::Use(use_stmt) => {
                    self.walk_use_statement(
                        use_stmt,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                    );
                }
                Statement::Expression(expr_stmt) => {
                    self.walk_expression(
                        expr_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Return(ret) => {
                    if let Some(expr) = &ret.value {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::If(if_stmt) => {
                    self.walk_expression(
                        &if_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        if_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    // Walk elseif clauses
                    for (cond, stmts) in if_stmt.body.else_if_clauses() {
                        self.walk_expression(
                            cond,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        self.walk_statement_slice(
                            stmts,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    // Walk else clause
                    if let Some(else_stmts) = if_stmt.body.else_statements() {
                        self.walk_statement_slice(
                            else_stmts,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Foreach(foreach_stmt) => {
                    self.walk_expression(
                        &foreach_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        foreach_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::While(while_stmt) => {
                    self.walk_expression(
                        &while_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        while_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::DoWhile(do_while_stmt) => {
                    self.walk_statement_slice(
                        std::slice::from_ref(do_while_stmt.statement),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_expression(
                        &do_while_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::For(for_stmt) => {
                    for expr in for_stmt.initializations.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    for expr in for_stmt.conditions.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    for expr in for_stmt.increments.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    self.walk_statement_slice(
                        for_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Try(try_stmt) => {
                    self.walk_block_statements(
                        &try_stmt.block,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    for catch_clause in try_stmt.catch_clauses.iter() {
                        self.walk_block_statements(
                            &catch_clause.block,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    if let Some(ref finally_clause) = try_stmt.finally_clause {
                        self.walk_block_statements(
                            &finally_clause.block,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Switch(switch_stmt) => {
                    self.walk_expression(
                        &switch_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    for case in switch_stmt.body.cases().iter() {
                        if let Some(expr) = case.expression() {
                            self.walk_expression(
                                expr,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        self.walk_statement_slice(
                            case.statements(),
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Block(block) => {
                    self.walk_block_statements(
                        block,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Echo(echo_stmt) => {
                    for expr in echo_stmt.values.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Unset(unset_stmt) => {
                    for expr in unset_stmt.values.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk a slice of statements (used for if/else/foreach/while/for/switch bodies).
    fn walk_statement_slice<'arena>(
        &self,
        statements: &[mago_syntax::ast::Statement<'arena>],
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
        enclosing_class_fqn: Option<&str>,
    ) {
        use mago_syntax::ast::Statement;

        for stmt in statements.iter() {
            match stmt {
                Statement::Namespace(ns) => {
                    self.walk_namespace(
                        ns,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Function(func) => {
                    self.walk_function(
                        func,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Class(class) => {
                    self.walk_class(
                        class,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Interface(iface) => {
                    self.walk_interface(
                        iface,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Trait(trait_def) => {
                    self.walk_trait(
                        trait_def,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Enum(enum_def) => {
                    self.walk_enum(
                        enum_def,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                    );
                }
                Statement::Constant(constant) => {
                    self.walk_constant(
                        constant,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                    );
                }
                Statement::Use(use_stmt) => {
                    self.walk_use_statement(
                        use_stmt,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                    );
                }
                Statement::Expression(expr_stmt) => {
                    self.walk_expression(
                        expr_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Return(ret) => {
                    if let Some(expr) = &ret.value {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::If(if_stmt) => {
                    self.walk_expression(
                        &if_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        if_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    for (cond, stmts) in if_stmt.body.else_if_clauses() {
                        self.walk_expression(
                            cond,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        self.walk_statement_slice(
                            stmts,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    if let Some(else_stmts) = if_stmt.body.else_statements() {
                        self.walk_statement_slice(
                            else_stmts,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Foreach(foreach_stmt) => {
                    self.walk_expression(
                        &foreach_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        foreach_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::While(while_stmt) => {
                    self.walk_expression(
                        &while_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_statement_slice(
                        while_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::DoWhile(do_while_stmt) => {
                    self.walk_statement_slice(
                        std::slice::from_ref(do_while_stmt.statement),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    self.walk_expression(
                        &do_while_stmt.condition,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::For(for_stmt) => {
                    for expr in for_stmt.initializations.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    for expr in for_stmt.conditions.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    for expr in for_stmt.increments.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    self.walk_statement_slice(
                        for_stmt.body.statements(),
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Try(try_stmt) => {
                    self.walk_block_statements(
                        &try_stmt.block,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    for catch_clause in try_stmt.catch_clauses.iter() {
                        self.walk_block_statements(
                            &catch_clause.block,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                    if let Some(ref finally_clause) = try_stmt.finally_clause {
                        self.walk_block_statements(
                            &finally_clause.block,
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Switch(switch_stmt) => {
                    self.walk_expression(
                        &switch_stmt.expression,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                    for case in switch_stmt.body.cases().iter() {
                        if let Some(expr) = case.expression() {
                            self.walk_expression(
                                expr,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        self.walk_statement_slice(
                            case.statements(),
                            trivia,
                            resolved_names,
                            builder,
                            line_index,
                            source,
                            occurrences,
                            symbols,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Block(block) => {
                    self.walk_block_statements(
                        block,
                        trivia,
                        resolved_names,
                        builder,
                        line_index,
                        source,
                        occurrences,
                        symbols,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                Statement::Echo(echo_stmt) => {
                    for expr in echo_stmt.values.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                Statement::Unset(unset_stmt) => {
                    for expr in unset_stmt.values.iter() {
                        self.walk_expression(
                            expr,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk a namespace and its body.
    fn walk_namespace<'arena>(
        &self,
        ns: &mago_syntax::ast::Namespace<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::NamespaceBody;

        // Emit namespace definition occurrence if namespace has a name
        if let Some(ref ns_name) = ns.name {
            let name_str = ns_name.value().to_string();
            let ns_symbol = builder.namespace_symbol(&name_str);
            let ns_symbol_str = format_symbol(&ns_symbol);

            let span = ns_name.span();
            occurrences.push(Occurrence {
                range: line_index.scip_range(span.start.offset, span.end.offset),
                symbol: ns_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: ns_symbol_str,
                kind: types::symbol_information::Kind::Namespace.into(),
                display_name: name_str,
                ..Default::default()
            });
        }

        // Walk namespace body
        match &ns.body {
            NamespaceBody::Implicit(body) => {
                self.walk_statements(
                    &body.statements,
                    trivia,
                    resolved_names,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                    local_counter,
                    None,
                );
            }
            NamespaceBody::BraceDelimited(block) => {
                self.walk_statements(
                    &block.statements,
                    trivia,
                    resolved_names,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                    local_counter,
                    None,
                );
            }
        }
    }

    /// Walk a class definition and its members.
    fn walk_class<'arena>(
        &self,
        class: &mago_syntax::ast::Class<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let class_name = class.name.value.to_string();
        let fqn = self
            .resolve_name(&class.name, resolved_names)
            .unwrap_or_else(|| class_name.clone());

        let class_symbol = builder.class_like_symbol(&fqn);
        let class_symbol_str = format_symbol(&class_symbol);

        // Definition occurrence for the class name
        let name_span = class.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: class_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        // Relationships (extends, implements)
        let mut relationships = Vec::new();
        if let Some(extends) = &class.extends {
            for parent in extends.types.iter() {
                let parent_fqn = self
                    .resolve_identifier(parent, resolved_names)
                    .unwrap_or_else(|| parent.value().to_string());
                let parent_sym = builder.class_like_symbol(&parent_fqn);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&parent_sym),
                    is_implementation: true,
                    ..Default::default()
                });

                // Reference occurrence for the parent class name
                let parent_span = parent.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(parent_span.start.offset, parent_span.end.offset),
                    symbol: format_symbol(&parent_sym),
                    ..Default::default()
                });
            }
        }
        if let Some(implements) = &class.implements {
            for iface in implements.types.iter() {
                let iface_fqn = self
                    .resolve_identifier(iface, resolved_names)
                    .unwrap_or_else(|| iface.value().to_string());
                let iface_sym = builder.class_like_symbol(&iface_fqn);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&iface_sym),
                    is_implementation: true,
                    ..Default::default()
                });

                let iface_span = iface.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(iface_span.start.offset, iface_span.end.offset),
                    symbol: format_symbol(&iface_sym),
                    ..Default::default()
                });
            }
        }

        let documentation = self.extract_documentation(trivia, class.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: class_symbol_str,
            kind: types::symbol_information::Kind::Class.into(),
            display_name: class_name,
            documentation,
            relationships,
            ..Default::default()
        });

        // Walk class members
        for member in class.members.iter() {
            self.walk_class_member(
                member,
                &fqn,
                trivia,
                resolved_names,
                builder,
                line_index,
                source,
                occurrences,
                symbols,
                local_counter,
            );
        }
    }

    /// Walk an interface definition.
    fn walk_interface<'arena>(
        &self,
        iface: &mago_syntax::ast::Interface<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let iface_name = iface.name.value.to_string();
        let fqn = self
            .resolve_name(&iface.name, resolved_names)
            .unwrap_or_else(|| iface_name.clone());

        let iface_symbol = builder.class_like_symbol(&fqn);
        let iface_symbol_str = format_symbol(&iface_symbol);

        let name_span = iface.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: iface_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let mut relationships = Vec::new();
        if let Some(extends) = &iface.extends {
            for parent in extends.types.iter() {
                let parent_fqn = self
                    .resolve_identifier(parent, resolved_names)
                    .unwrap_or_else(|| parent.value().to_string());
                let parent_sym = builder.class_like_symbol(&parent_fqn);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&parent_sym),
                    is_implementation: true,
                    ..Default::default()
                });
                let parent_span = parent.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(parent_span.start.offset, parent_span.end.offset),
                    symbol: format_symbol(&parent_sym),
                    ..Default::default()
                });
            }
        }

        let documentation = self.extract_documentation(trivia, iface.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: iface_symbol_str,
            kind: types::symbol_information::Kind::Interface.into(),
            display_name: iface_name,
            documentation,
            relationships,
            ..Default::default()
        });

        for member in iface.members.iter() {
            self.walk_class_member(
                member,
                &fqn,
                trivia,
                resolved_names,
                builder,
                line_index,
                source,
                occurrences,
                symbols,
                local_counter,
            );
        }
    }

    /// Walk a trait definition.
    fn walk_trait<'arena>(
        &self,
        trait_def: &mago_syntax::ast::Trait<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let trait_name = trait_def.name.value.to_string();
        let fqn = self
            .resolve_name(&trait_def.name, resolved_names)
            .unwrap_or_else(|| trait_name.clone());

        let trait_symbol = builder.class_like_symbol(&fqn);
        let trait_symbol_str = format_symbol(&trait_symbol);

        let name_span = trait_def.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: trait_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let documentation =
            self.extract_documentation(trivia, trait_def.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: trait_symbol_str,
            kind: types::symbol_information::Kind::Trait.into(),
            display_name: trait_name,
            documentation,
            ..Default::default()
        });

        for member in trait_def.members.iter() {
            self.walk_class_member(
                member,
                &fqn,
                trivia,
                resolved_names,
                builder,
                line_index,
                source,
                occurrences,
                symbols,
                local_counter,
            );
        }
    }

    /// Walk an enum definition.
    fn walk_enum<'arena>(
        &self,
        enum_def: &mago_syntax::ast::Enum<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let enum_name = enum_def.name.value.to_string();
        let fqn = self
            .resolve_name(&enum_def.name, resolved_names)
            .unwrap_or_else(|| enum_name.clone());

        let enum_symbol = builder.class_like_symbol(&fqn);
        let enum_symbol_str = format_symbol(&enum_symbol);

        let name_span = enum_def.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: enum_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let mut relationships = Vec::new();
        if let Some(implements) = &enum_def.implements {
            for iface in implements.types.iter() {
                let iface_fqn = self
                    .resolve_identifier(iface, resolved_names)
                    .unwrap_or_else(|| iface.value().to_string());
                let iface_sym = builder.class_like_symbol(&iface_fqn);
                relationships.push(types::Relationship {
                    symbol: format_symbol(&iface_sym),
                    is_implementation: true,
                    ..Default::default()
                });
                let iface_span = iface.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(iface_span.start.offset, iface_span.end.offset),
                    symbol: format_symbol(&iface_sym),
                    ..Default::default()
                });
            }
        }

        let documentation =
            self.extract_documentation(trivia, enum_def.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: enum_symbol_str,
            kind: types::symbol_information::Kind::Enum.into(),
            display_name: enum_name,
            documentation,
            relationships,
            ..Default::default()
        });

        // Walk enum members (uses ClassLikeMember, EnumCase is a variant)
        for member in enum_def.members.iter() {
            self.walk_class_member(
                member,
                &fqn,
                trivia,
                resolved_names,
                builder,
                line_index,
                source,
                occurrences,
                symbols,
                local_counter,
            );
        }
    }

    /// Walk a class/interface/trait/enum member.
    fn walk_class_member<'arena>(
        &self,
        member: &mago_syntax::ast::ClassLikeMember<'arena>,
        class_fqn: &str,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        use mago_syntax::ast::ClassLikeMember;
        match member {
            ClassLikeMember::Method(method) => {
                self.walk_method(
                    method,
                    class_fqn,
                    trivia,
                    resolved_names,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                    local_counter,
                );
            }
            ClassLikeMember::Property(prop) => {
                self.walk_property(
                    prop,
                    class_fqn,
                    trivia,
                    resolved_names,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                );
            }
            ClassLikeMember::Constant(constant) => {
                self.walk_class_constant(
                    constant,
                    class_fqn,
                    trivia,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                );
            }
            ClassLikeMember::EnumCase(case) => {
                self.walk_enum_case(
                    case,
                    class_fqn,
                    trivia,
                    builder,
                    line_index,
                    source,
                    occurrences,
                    symbols,
                );
            }
            ClassLikeMember::TraitUse(trait_use) => {
                // Emit references for used traits
                for trait_name in trait_use.trait_names.iter() {
                    let fqn = self
                        .resolve_identifier(trait_name, resolved_names)
                        .unwrap_or_else(|| trait_name.value().to_string());
                    let trait_sym = builder.class_like_symbol(&fqn);
                    let trait_span = trait_name.span();
                    occurrences.push(Occurrence {
                        range: line_index
                            .scip_range(trait_span.start.offset, trait_span.end.offset),
                        symbol: format_symbol(&trait_sym),
                        ..Default::default()
                    });
                }
            }
        }
    }

    /// Walk an enum case.
    fn walk_enum_case<'arena>(
        &self,
        case: &mago_syntax::ast::EnumCase<'arena>,
        enum_fqn: &str,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        let case_name_ident = case.item.name();
        let case_name = case_name_ident.value.to_string();
        let case_symbol = builder.enum_case_symbol(enum_fqn, &case_name);
        let case_symbol_str = format_symbol(&case_symbol);

        let name_span = case_name_ident.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: case_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let documentation = self.extract_documentation(trivia, case.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: case_symbol_str,
            kind: types::symbol_information::Kind::EnumMember.into(),
            display_name: case_name,
            documentation,
            ..Default::default()
        });
    }

    /// Walk a method definition.
    fn walk_method<'arena>(
        &self,
        method: &mago_syntax::ast::Method<'arena>,
        class_fqn: &str,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let method_name = method.name.value.to_string();
        let method_symbol = builder.method_symbol(class_fqn, &method_name);
        let method_symbol_str = format_symbol(&method_symbol);

        let name_span = method.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: method_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let kind = if method_name == "__construct" {
            types::symbol_information::Kind::Constructor
        } else {
            types::symbol_information::Kind::Method
        };

        let documentation = self.extract_documentation(trivia, method.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: method_symbol_str,
            kind: kind.into(),
            display_name: method_name.clone(),
            documentation,
            ..Default::default()
        });

        // Walk parameters
        for param in method.parameter_list.parameters.iter() {
            let param_name = param
                .variable
                .name
                .strip_prefix('$')
                .unwrap_or(param.variable.name)
                .to_string();
            let param_sym = builder.parameter_symbol(class_fqn, &method_name, &param_name);
            let param_symbol_str = format_symbol(&param_sym);

            let param_span = param.variable.span;
            occurrences.push(Occurrence {
                range: line_index.scip_range(param_span.start.offset, param_span.end.offset),
                symbol: param_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: param_symbol_str,
                kind: types::symbol_information::Kind::Parameter.into(),
                display_name: format!("${}", param_name),
                ..Default::default()
            });

            // Emit reference for parameter type hint
            if let Some(hint) = &param.hint {
                self.walk_hint(hint, resolved_names, builder, line_index, occurrences);
            }
        }

        // Emit reference for return type
        if let Some(return_type) = &method.return_type_hint {
            self.walk_hint(
                &return_type.hint,
                resolved_names,
                builder,
                line_index,
                occurrences,
            );
        }

        // Walk method body for expression references
        if let mago_syntax::ast::MethodBody::Concrete(ref block) = method.body {
            // Populate variable type map from parameter type hints
            self.populate_var_types_from_params(&method.parameter_list, resolved_names, Some(class_fqn));
            self.walk_block_statements(
                block,
                trivia,
                resolved_names,
                builder,
                line_index,
                source,
                occurrences,
                symbols,
                local_counter,
                Some(class_fqn),
            );
            self.var_types.borrow_mut().clear();
        }
    }

    /// Walk a function definition (top-level).
    fn walk_function<'arena>(
        &self,
        func: &mago_syntax::ast::Function<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
    ) {
        let func_name = func.name.value.to_string();
        let fqn = self
            .resolve_name(&func.name, resolved_names)
            .unwrap_or_else(|| func_name.clone());

        let func_symbol = builder.function_symbol(&fqn);
        let func_symbol_str = format_symbol(&func_symbol);

        let name_span = func.name.span;
        occurrences.push(Occurrence {
            range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
            symbol: func_symbol_str.clone(),
            symbol_roles: types::SymbolRole::Definition as i32,
            ..Default::default()
        });

        let documentation = self.extract_documentation(trivia, func.span().start.offset, source);
        symbols.push(SymbolInformation {
            symbol: func_symbol_str,
            kind: types::symbol_information::Kind::Function.into(),
            display_name: func_name.clone(),
            documentation,
            ..Default::default()
        });

        // Walk parameters
        for param in func.parameter_list.parameters.iter() {
            let param_name = param
                .variable
                .name
                .strip_prefix('$')
                .unwrap_or(param.variable.name)
                .to_string();
            let param_sym = builder.function_parameter_symbol(&fqn, &param_name);
            let param_symbol_str = format_symbol(&param_sym);

            let param_span = param.variable.span;
            occurrences.push(Occurrence {
                range: line_index.scip_range(param_span.start.offset, param_span.end.offset),
                symbol: param_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            symbols.push(SymbolInformation {
                symbol: param_symbol_str,
                kind: types::symbol_information::Kind::Parameter.into(),
                display_name: format!("${}", param_name),
                ..Default::default()
            });

            if let Some(hint) = &param.hint {
                self.walk_hint(hint, resolved_names, builder, line_index, occurrences);
            }
        }

        if let Some(return_type) = &func.return_type_hint {
            self.walk_hint(
                &return_type.hint,
                resolved_names,
                builder,
                line_index,
                occurrences,
            );
        }

        // Walk function body for expression references
        self.populate_var_types_from_params(&func.parameter_list, resolved_names, None);
        self.walk_block_statements(
            &func.body,
            trivia,
            resolved_names,
            builder,
            line_index,
            source,
            occurrences,
            symbols,
            local_counter,
            None,
        );
        self.var_types.borrow_mut().clear();
    }

    /// Walk a property definition.
    fn walk_property<'arena>(
        &self,
        prop: &mago_syntax::ast::Property<'arena>,
        class_fqn: &str,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        // Emit type hint reference if present
        if let Some(hint) = prop.hint() {
            self.walk_hint(hint, resolved_names, builder, line_index, occurrences);
        }

        // Emit definition for each variable
        for var in prop.variables() {
            let var_name = var.name.strip_prefix('$').unwrap_or(var.name).to_string();
            let prop_sym = builder.property_symbol(class_fqn, &var_name);
            let prop_symbol_str = format_symbol(&prop_sym);

            let var_span = var.span;
            occurrences.push(Occurrence {
                range: line_index.scip_range(var_span.start.offset, var_span.end.offset),
                symbol: prop_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            let documentation =
                self.extract_documentation(trivia, prop.span().start.offset, source);
            symbols.push(SymbolInformation {
                symbol: prop_symbol_str,
                kind: types::symbol_information::Kind::Property.into(),
                display_name: format!("${}", var_name),
                documentation,
                ..Default::default()
            });
        }
    }

    /// Walk a class constant definition.
    fn walk_class_constant<'arena>(
        &self,
        constant: &mago_syntax::ast::ClassLikeConstant<'arena>,
        class_fqn: &str,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        for item in constant.items.iter() {
            let const_name = item.name.value.to_string();
            let const_sym = builder.class_constant_symbol(class_fqn, &const_name);
            let const_symbol_str = format_symbol(&const_sym);

            let name_span = item.name.span;
            occurrences.push(Occurrence {
                range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
                symbol: const_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            let documentation =
                self.extract_documentation(trivia, constant.span().start.offset, source);
            symbols.push(SymbolInformation {
                symbol: const_symbol_str,
                kind: types::symbol_information::Kind::Constant.into(),
                display_name: const_name,
                documentation,
                ..Default::default()
            });
        }
    }

    /// Walk a top-level constant.
    fn walk_constant<'arena>(
        &self,
        constant: &mago_syntax::ast::Constant<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
    ) {
        for item in constant.items.iter() {
            let const_name = item.name.value.to_string();
            let fqn = self
                .resolve_name(&item.name, resolved_names)
                .unwrap_or_else(|| const_name.clone());

            let const_sym = builder.constant_symbol(&fqn);
            let const_symbol_str = format_symbol(&const_sym);

            let name_span = item.name.span;
            occurrences.push(Occurrence {
                range: line_index.scip_range(name_span.start.offset, name_span.end.offset),
                symbol: const_symbol_str.clone(),
                symbol_roles: types::SymbolRole::Definition as i32,
                ..Default::default()
            });

            let documentation =
                self.extract_documentation(trivia, constant.span().start.offset, source);
            symbols.push(SymbolInformation {
                symbol: const_symbol_str,
                kind: types::symbol_information::Kind::Constant.into(),
                display_name: const_name,
                documentation,
                ..Default::default()
            });
        }
    }

    /// Walk a type hint to emit references to named types.
    fn walk_hint<'arena>(
        &self,
        hint: &mago_syntax::ast::Hint<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        occurrences: &mut Vec<Occurrence>,
    ) {
        use mago_syntax::ast::Hint;
        match hint {
            Hint::Identifier(ident) => {
                // Named type like User, \App\Models\User, etc.
                // Skip built-in types
                let name = ident.value();
                if !is_builtin_type(name) {
                    let fqn = self
                        .resolve_identifier(ident, resolved_names)
                        .unwrap_or_else(|| name.to_string());
                    let sym = builder.class_like_symbol(&fqn);
                    let span = ident.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(span.start.offset, span.end.offset),
                        symbol: format_symbol(&sym),
                        ..Default::default()
                    });
                }
            }
            Hint::Nullable(nullable) => {
                self.walk_hint(
                    &nullable.hint,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
            }
            Hint::Union(union) => {
                self.walk_hint(
                    &union.left,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
                self.walk_hint(
                    &union.right,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
            }
            Hint::Intersection(intersection) => {
                self.walk_hint(
                    &intersection.left,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
                self.walk_hint(
                    &intersection.right,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
            }
            Hint::Parenthesized(parens) => {
                self.walk_hint(
                    &parens.hint,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                );
            }
            _ => {
                // Built-in types: Void, Bool, Int, String, etc. -- no reference needed
            }
        }
    }

    /// Walk use/import statements.
    fn walk_use_statement<'arena>(
        &self,
        use_stmt: &mago_syntax::ast::Use<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        occurrences: &mut Vec<Occurrence>,
    ) {
        use mago_syntax::ast::UseItems;
        use mago_syntax::ast::UseType;

        // Helper closure to emit an import occurrence for a UseItem.
        // `use_type` indicates whether this is `use function`, `use const`, or plain `use`.
        let mut emit_use_item =
            |item: &mago_syntax::ast::UseItem<'arena>, use_type: Option<&UseType<'arena>>| {
                let fqn = self
                    .resolve_identifier(&item.name, resolved_names)
                    .unwrap_or_else(|| item.name.value().to_string());

                let sym = match use_type {
                    Some(UseType::Function(_)) => builder.function_symbol(&fqn),
                    Some(UseType::Const(_)) => builder.constant_symbol(&fqn),
                    None => builder.class_like_symbol(&fqn),
                };
                let span = item.name.span();
                occurrences.push(Occurrence {
                    range: line_index.scip_range(span.start.offset, span.end.offset),
                    symbol: format_symbol(&sym),
                    symbol_roles: types::SymbolRole::Import as i32,
                    ..Default::default()
                });
            };

        match &use_stmt.items {
            UseItems::Sequence(seq) => {
                for item in seq.items.iter() {
                    emit_use_item(item, None);
                }
            }
            UseItems::TypedSequence(seq) => {
                for item in seq.items.iter() {
                    emit_use_item(item, Some(&seq.r#type));
                }
            }
            UseItems::TypedList(list) => {
                for item in list.items.iter() {
                    emit_use_item(item, Some(&list.r#type));
                }
            }
            UseItems::MixedList(list) => {
                for maybe_typed in list.items.iter() {
                    emit_use_item(&maybe_typed.item, maybe_typed.r#type.as_ref());
                }
            }
        }
    }

    /// Walk an expression to find references.
    fn walk_expression<'arena>(
        &self,
        expr: &mago_syntax::ast::Expression<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        occurrences: &mut Vec<Occurrence>,
        local_counter: &mut usize,
        enclosing_class_fqn: Option<&str>,
    ) {
        use mago_syntax::ast::Expression;
        match expr {
            // new ClassName(...)
            Expression::Instantiation(inst) => {
                self.walk_expression(
                    inst.class,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
                if let Some(arg_list) = &inst.argument_list {
                    for arg in arg_list.arguments.iter() {
                        self.walk_expression(
                            arg.value(),
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                    }
                }
            }

            // Function/method/static method calls
            Expression::Call(call) => {
                use mago_syntax::ast::Call;
                match call {
                    Call::Function(func_call) => {
                        // If the function expression is a bare identifier, emit a
                        // function_symbol (with `().` suffix) instead of delegating
                        // to walk_expression which would emit class_like_symbol (`#`).
                        if let Expression::Identifier(ident) = func_call.function {
                            let name = ident.value();
                            if !is_builtin_type(name) {
                                let fqn = self
                                    .resolve_identifier(ident, resolved_names)
                                    .unwrap_or_else(|| name.to_string());
                                let sym = builder.function_symbol(&fqn);
                                let span = ident.span();
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        } else {
                            // Dynamic/complex function expression (e.g. $var(), $obj->getCallable()())
                            self.walk_expression(
                                func_call.function,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        for arg in func_call.argument_list.arguments.iter() {
                            self.walk_expression(
                                arg.value(),
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                    }
                    Call::StaticMethod(static_call) => {
                        // Reference to the class
                        self.walk_expression(
                            static_call.class,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the method name (e.g. ClassName::methodName())
                        if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                            &static_call.method
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                static_call.class,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let method_name = method_ident.value;
                                let sym = builder.method_symbol(&class_fqn, method_name);
                                let span = method_ident.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                        for arg in static_call.argument_list.arguments.iter() {
                            self.walk_expression(
                                arg.value(),
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                    }
                    Call::Method(method_call) => {
                        self.walk_expression(
                            method_call.object,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the method name (e.g. $this->methodName())
                        if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                            &method_call.method
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                method_call.object,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let method_name = method_ident.value;
                                let sym = builder.method_symbol(&class_fqn, method_name);
                                let span = method_ident.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                        for arg in method_call.argument_list.arguments.iter() {
                            self.walk_expression(
                                arg.value(),
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                    }
                    Call::NullSafeMethod(method_call) => {
                        self.walk_expression(
                            method_call.object,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the method name (e.g. $obj?->methodName())
                        if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                            &method_call.method
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                method_call.object,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let method_name = method_ident.value;
                                let sym = builder.method_symbol(&class_fqn, method_name);
                                let span = method_ident.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                        for arg in method_call.argument_list.arguments.iter() {
                            self.walk_expression(
                                arg.value(),
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                    }
                }
            }

            // Property/constant access
            Expression::Access(access) => {
                use mago_syntax::ast::Access;
                match access {
                    Access::Property(prop) => {
                        self.walk_expression(
                            prop.object,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the property name (e.g. $this->propertyName)
                        if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(prop_ident) =
                            &prop.property
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                prop.object,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let prop_name = prop_ident.value;
                                let sym = builder.property_symbol(&class_fqn, prop_name);
                                let span = prop_ident.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    Access::NullSafeProperty(prop) => {
                        self.walk_expression(
                            prop.object,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the property name (e.g. $obj?->propertyName)
                        if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(prop_ident) =
                            &prop.property
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                prop.object,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let prop_name = prop_ident.value;
                                let sym = builder.property_symbol(&class_fqn, prop_name);
                                let span = prop_ident.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    Access::StaticProperty(prop) => {
                        self.walk_expression(
                            prop.class,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the static property (e.g. ClassName::$propName)
                        if let mago_syntax::ast::Variable::Direct(var) = &prop.property {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                prop.class,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let prop_name = var.name.strip_prefix('$').unwrap_or(var.name);
                                let sym = builder.property_symbol(&class_fqn, prop_name);
                                let span = var.span;
                                occurrences.push(Occurrence {
                                    range: line_index
                                        .scip_range(span.start.offset, span.end.offset),
                                    symbol: format_symbol(&sym),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    Access::ClassConstant(cc) => {
                        self.walk_expression(
                            cc.class,
                            resolved_names,
                            builder,
                            line_index,
                            occurrences,
                            local_counter,
                            enclosing_class_fqn,
                        );
                        // Emit reference for the constant name (e.g. ClassName::CONST_NAME)
                        if let mago_syntax::ast::ClassLikeConstantSelector::Identifier(
                            const_ident,
                        ) = &cc.constant
                        {
                            if let Some(class_fqn) = self.try_resolve_class_from_expr(
                                cc.class,
                                resolved_names,
                                enclosing_class_fqn,
                            ) {
                                let const_name = const_ident.value;
                                // Skip "class" pseudo-constant (ClassName::class)
                                if const_name != "class" {
                                    let sym = builder.class_constant_symbol(&class_fqn, const_name);
                                    let span = const_ident.span;
                                    occurrences.push(Occurrence {
                                        range: line_index
                                            .scip_range(span.start.offset, span.end.offset),
                                        symbol: format_symbol(&sym),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Identifier (class name, function name references)
            Expression::Identifier(ident) => {
                let name = ident.value();
                if !is_builtin_type(name) {
                    let fqn = self
                        .resolve_identifier(ident, resolved_names)
                        .unwrap_or_else(|| name.to_string());
                    let sym = builder.class_like_symbol(&fqn);
                    let span = ident.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(span.start.offset, span.end.offset),
                        symbol: format_symbol(&sym),
                        ..Default::default()
                    });
                }
            }

            // Assignment: walk both sides, and infer variable types from RHS expressions.
            // Handles `new ClassName()`, `$obj->method()`, `ClassName::method()`,
            // `$obj->property`, etc., by delegating to try_resolve_class_from_expr.
            Expression::Assignment(assign) => {
                // If LHS is a simple variable, infer its type from the RHS expression.
                if let Expression::Variable(mago_syntax::ast::Variable::Direct(dv)) = assign.lhs {
                    if let Some(class_fqn) = self.try_resolve_class_from_expr(
                        assign.rhs,
                        resolved_names,
                        enclosing_class_fqn,
                    ) {
                        self.var_types
                            .borrow_mut()
                            .insert(dv.name.to_string(), class_fqn);
                    } else {
                        self.var_types
                            .borrow_mut()
                            .remove(&dv.name.to_string());
                    }
                }
                self.walk_expression(
                    assign.lhs,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
                self.walk_expression(
                    assign.rhs,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Binary expressions
            Expression::Binary(binary) => {
                self.walk_expression(
                    binary.lhs,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
                self.walk_expression(
                    binary.rhs,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Unary
            Expression::UnaryPrefix(unary) => {
                self.walk_expression(
                    unary.operand,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }
            Expression::UnaryPostfix(unary) => {
                self.walk_expression(
                    unary.operand,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Parenthesized
            Expression::Parenthesized(parens) => {
                self.walk_expression(
                    parens.expression,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Conditional (ternary)
            Expression::Conditional(cond) => {
                self.walk_expression(
                    cond.condition,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
                if let Some(then_expr) = &cond.then {
                    self.walk_expression(
                        then_expr,
                        resolved_names,
                        builder,
                        line_index,
                        occurrences,
                        local_counter,
                        enclosing_class_fqn,
                    );
                }
                self.walk_expression(
                    cond.r#else,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Array elements
            Expression::Array(array) => {
                for element in array.elements.iter() {
                    use mago_syntax::ast::ArrayElement;
                    match element {
                        ArrayElement::KeyValue(kv) => {
                            self.walk_expression(
                                kv.key,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                            self.walk_expression(
                                kv.value,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        ArrayElement::Value(val) => {
                            self.walk_expression(
                                val.value,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        ArrayElement::Variadic(var) => {
                            self.walk_expression(
                                var.value,
                                resolved_names,
                                builder,
                                line_index,
                                occurrences,
                                local_counter,
                                enclosing_class_fqn,
                            );
                        }
                        _ => {}
                    }
                }
            }

            // Closure and arrow function - walk body references
            Expression::Closure(closure) => {
                if let Some(hint) = &closure.return_type_hint {
                    self.walk_hint(&hint.hint, resolved_names, builder, line_index, occurrences);
                }
                for param in closure.parameter_list.parameters.iter() {
                    if let Some(hint) = &param.hint {
                        self.walk_hint(hint, resolved_names, builder, line_index, occurrences);
                    }
                }
            }
            Expression::ArrowFunction(arrow) => {
                if let Some(hint) = &arrow.return_type_hint {
                    self.walk_hint(&hint.hint, resolved_names, builder, line_index, occurrences);
                }
                for param in arrow.parameter_list.parameters.iter() {
                    if let Some(hint) = &param.hint {
                        self.walk_hint(hint, resolved_names, builder, line_index, occurrences);
                    }
                }
                self.walk_expression(
                    arrow.expression,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Throw
            Expression::Throw(throw) => {
                self.walk_expression(
                    throw.exception,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Clone
            Expression::Clone(clone) => {
                self.walk_expression(
                    clone.object,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // ArrayAccess: $arr[$key]
            Expression::ArrayAccess(access) => {
                self.walk_expression(
                    access.array,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
                self.walk_expression(
                    access.index,
                    resolved_names,
                    builder,
                    line_index,
                    occurrences,
                    local_counter,
                    enclosing_class_fqn,
                );
            }

            // Constant access (bare names like PHP_EOL)
            Expression::ConstantAccess(ca) => {
                let name = ca.name.value();
                if !is_builtin_type(name) && !is_builtin_constant(name) {
                    let fqn = self
                        .resolve_identifier(&ca.name, resolved_names)
                        .unwrap_or_else(|| name.to_string());
                    let sym = builder.constant_symbol(&fqn);
                    let span = ca.name.span();
                    occurrences.push(Occurrence {
                        range: line_index.scip_range(span.start.offset, span.end.offset),
                        symbol: format_symbol(&sym),
                        ..Default::default()
                    });
                }
            }

            // Variable, Literal, self/static/parent, magic constants — no further references
            _ => {}
        }
    }

    /// Walk statements in a block (used for method/function bodies).
    fn walk_block_statements<'arena>(
        &self,
        block: &mago_syntax::ast::Block<'arena>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        builder: &SymbolBuilder<'_>,
        line_index: &LineIndex,
        source: &str,
        occurrences: &mut Vec<Occurrence>,
        symbols: &mut Vec<SymbolInformation>,
        local_counter: &mut usize,
        enclosing_class_fqn: Option<&str>,
    ) {
        self.walk_statements(
            &block.statements,
            trivia,
            resolved_names,
            builder,
            line_index,
            source,
            occurrences,
            symbols,
            local_counter,
            enclosing_class_fqn,
        );
    }

    // --- Helper methods ---

    /// Extract PHPDoc documentation for a definition at the given span.
    /// Finds the closest preceding docblock comment in the trivia and parses it.
    fn extract_documentation<'arena>(
        &self,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        def_offset: u32,
        source: &str,
    ) -> Vec<String> {
        use mago_syntax::ast::TriviaKind;

        // Find the closest docblock that ends before the definition starts
        let mut best: Option<&mago_syntax::ast::Trivia<'arena>> = None;
        for t in trivia.iter() {
            if t.kind != TriviaKind::DocBlockComment {
                continue;
            }
            if t.span.end.offset <= def_offset {
                match best {
                    Some(prev) if t.span.end.offset > prev.span.end.offset => {
                        best = Some(t);
                    }
                    None => {
                        best = Some(t);
                    }
                    _ => {}
                }
            }
        }

        let Some(docblock_trivia) = best else {
            return Vec::new();
        };

        // Ensure the docblock is directly attached to this definition.
        // Check the source text between docblock end and definition start:
        // if it contains '{', '}', or ';' then another statement exists between them.
        let docblock_end = docblock_trivia.span.end.offset;
        let between = &source[docblock_end as usize..def_offset as usize];
        if between.contains('{') || between.contains('}') || between.contains(';') {
            return Vec::new();
        }

        let arena = bumpalo::Bump::new();
        let doc = match mago_docblock::parse_trivia(&arena, docblock_trivia) {
            Ok(doc) => doc,
            Err(_) => return Vec::new(),
        };

        let mut documentation = Vec::new();
        for element in doc.elements.iter() {
            match element {
                mago_docblock::document::Element::Text(text) => {
                    let mut buf = String::new();
                    for segment in text.segments.iter() {
                        match segment {
                            mago_docblock::document::TextSegment::Paragraph { content, .. } => {
                                buf.push_str(content);
                            }
                            mago_docblock::document::TextSegment::InlineCode(code) => {
                                buf.push('`');
                                buf.push_str(code.content);
                                buf.push('`');
                            }
                            mago_docblock::document::TextSegment::InlineTag(tag) => {
                                buf.push_str(&format!("@{} {}", tag.name, tag.description));
                            }
                        }
                    }
                    if !buf.trim().is_empty() {
                        documentation.push(buf.trim().to_string());
                    }
                }
                mago_docblock::document::Element::Code(code) => {
                    let lang = if code.directives.is_empty() {
                        ""
                    } else {
                        code.directives.first().copied().unwrap_or("")
                    };
                    documentation.push(format!("```{}\n{}\n```", lang, code.content));
                }
                mago_docblock::document::Element::Tag(tag) => {
                    documentation.push(format!("@{} {}", tag.name, tag.description));
                }
                _ => {}
            }
        }

        documentation
    }

    /// Resolve a LocalIdentifier using the resolved names map.
    /// Returns the fully-qualified name if found, None otherwise.
    fn resolve_name(
        &self,
        ident: &mago_syntax::ast::LocalIdentifier<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
    ) -> Option<String> {
        resolved_names.resolve(ident).map(|s| s.to_string())
    }

    /// Resolve an Identifier (Local/Qualified/FullyQualified) using resolved names.
    fn resolve_identifier(
        &self,
        ident: &mago_syntax::ast::Identifier<'_>,
        resolved_names: &mago_names::ResolvedNames<'_>,
    ) -> Option<String> {
        resolved_names.resolve(ident).map(|s| s.to_string())
    }

    /// Try to resolve a class FQN from an expression used as a class reference.
    ///
    /// Handles:
    /// - `$this` → uses `enclosing_class_fqn`
    /// - `self`, `static` → uses `enclosing_class_fqn`
    /// - `parent` → returns `None` (parent FQN resolution not yet implemented)
    /// - `ClassName` (Identifier) → resolves via resolved_names
    /// - `$variable` → looks up type from parameter type hints in `var_types`
    fn try_resolve_class_from_expr<'arena>(
        &self,
        expr: &mago_syntax::ast::Expression<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        enclosing_class_fqn: Option<&str>,
    ) -> Option<String> {
        use mago_syntax::ast::{Access, Expression, Variable};

        match expr {
            Expression::Variable(Variable::Direct(dv)) if dv.name == "$this" => {
                enclosing_class_fqn.map(|s| s.to_string())
            }
            Expression::Variable(Variable::Direct(dv)) => {
                // Look up variable type from parameter type hints
                self.var_types.borrow().get(dv.name).cloned()
            }
            // `self` keyword → refers to the enclosing class
            Expression::Self_(_) => enclosing_class_fqn.map(|s| s.to_string()),
            // `static` keyword → late-static binding, treat same as self for resolution
            Expression::Static(_) => enclosing_class_fqn.map(|s| s.to_string()),
            Expression::Identifier(ident) => {
                let name = ident.value();
                match name.to_lowercase().as_str() {
                    "self" | "static" => enclosing_class_fqn.map(|s| s.to_string()),
                    "parent" => None,
                    _ => {
                        let fqn = self
                            .resolve_identifier(ident, resolved_names)
                            .unwrap_or_else(|| name.to_string());
                        Some(fqn)
                    }
                }
            }
            // Chained property access: `$device->home` or `$device?->home`
            // Resolve the object's type first, then look up the property in the
            // pre-built property_types map to obtain the property's own type.
            Expression::Access(Access::Property(prop)) => {
                if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(prop_ident) =
                    &prop.property
                {
                    let object_class = self.try_resolve_class_from_expr(
                        prop.object,
                        resolved_names,
                        enclosing_class_fqn,
                    )?;
                    self.property_types
                        .borrow()
                        .get(&object_class)?
                        .get(prop_ident.value)
                        .cloned()
                } else {
                    None
                }
            }
            Expression::Access(Access::NullSafeProperty(prop)) => {
                if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(prop_ident) =
                    &prop.property
                {
                    let object_class = self.try_resolve_class_from_expr(
                        prop.object,
                        resolved_names,
                        enclosing_class_fqn,
                    )?;
                    self.property_types
                        .borrow()
                        .get(&object_class)?
                        .get(prop_ident.value)
                        .cloned()
                } else {
                    None
                }
            }
            // `new ClassName(...)` → resolve the class reference
            Expression::Instantiation(inst) => {
                self.try_resolve_class_from_expr(inst.class, resolved_names, enclosing_class_fqn)
            }
            // Method call: `$obj->method(...)` → look up method return type
            Expression::Call(mago_syntax::ast::Call::Method(mc)) => {
                if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                    &mc.method
                {
                    let object_class = self.try_resolve_class_from_expr(
                        mc.object,
                        resolved_names,
                        enclosing_class_fqn,
                    )?;
                    self.method_return_types
                        .borrow()
                        .get(&object_class)?
                        .get(method_ident.value)
                        .cloned()
                } else {
                    None
                }
            }
            // Null-safe method call: `$obj?->method(...)` → same as above
            Expression::Call(mago_syntax::ast::Call::NullSafeMethod(mc)) => {
                if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                    &mc.method
                {
                    let object_class = self.try_resolve_class_from_expr(
                        mc.object,
                        resolved_names,
                        enclosing_class_fqn,
                    )?;
                    self.method_return_types
                        .borrow()
                        .get(&object_class)?
                        .get(method_ident.value)
                        .cloned()
                } else {
                    None
                }
            }
            // Static method call: `ClassName::method(...)` → look up method return type
            Expression::Call(mago_syntax::ast::Call::StaticMethod(sc)) => {
                if let mago_syntax::ast::ClassLikeMemberSelector::Identifier(method_ident) =
                    &sc.method
                {
                    let class_fqn = self.try_resolve_class_from_expr(
                        sc.class,
                        resolved_names,
                        enclosing_class_fqn,
                    )?;
                    self.method_return_types
                        .borrow()
                        .get(&class_fqn)?
                        .get(method_ident.value)
                        .cloned()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Try to extract a class FQN from a type hint.
    /// Returns `Some(fqn)` for simple class type hints (including nullable),
    /// `None` for union/intersection types, built-in types, etc.
    fn resolve_hint_to_fqn<'arena>(
        &self,
        hint: &mago_syntax::ast::Hint<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        class_fqn: Option<&str>,
    ) -> Option<String> {
        use mago_syntax::ast::Hint;
        match hint {
            Hint::Identifier(ident) => {
                let name = ident.value();
                // Translate fluent return types to the enclosing class FQN.
                if matches!(name.to_lowercase().as_str(), "self" | "static") {
                    return class_fqn.map(String::from);
                }
                if is_builtin_type(name) {
                    None
                } else {
                    let fqn = self
                        .resolve_identifier(ident, resolved_names)
                        .unwrap_or_else(|| name.to_string());
                    Some(fqn)
                }
            }
            Hint::Nullable(nullable) => {
                self.resolve_hint_to_fqn(&nullable.hint, resolved_names, class_fqn)
            }
            // Union/intersection types are ambiguous — don't resolve
            _ => None,
        }
    }

    /// Populate `var_types` from parameter type hints of a function-like parameter list.
    fn populate_var_types_from_params<'arena>(
        &self,
        params: &mago_syntax::ast::FunctionLikeParameterList<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        class_fqn: Option<&str>,
    ) {
        let mut var_types = self.var_types.borrow_mut();
        for param in params.parameters.iter() {
            if let Some(hint) = &param.hint {
                if let Some(fqn) = self.resolve_hint_to_fqn(hint, resolved_names, class_fqn) {
                    var_types.insert(param.variable.name.to_string(), fqn);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Property-type pre-pass
    // -----------------------------------------------------------------------

    /// Lightweight pre-pass over a single PHP file: collect class property type
    /// hints and method return types into `self.property_types` /
    /// `self.method_return_types`.  Parse errors are silently skipped.
    fn collect_property_types_from_file(&self, file_path: &std::path::Path) {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => return,
        };

        let arena = bumpalo::Bump::new();
        let relative_path = self.project.relative_path(file_path);
        let file =
            mago_database::file::File::ephemeral(relative_path.into(), source.clone().into());
        let program = mago_syntax::parser::parse_file(&arena, &file);

        let resolver = mago_names::resolver::NameResolver::new(&arena);
        let resolved_names = resolver.resolve(program);

        // Build a simple use-alias map for PHPDoc type resolution.
        // Maps last-segment (or alias) → fully-qualified name.
        let use_map = self.build_use_map_from_statements(&program.statements);

        self.collect_property_types_from_statements(
            &program.statements,
            &program.trivia,
            program.source_text,
            &resolved_names,
            &use_map,
        );
    }

    /// Walk use-statement items and build a map from the last name segment (or
    /// explicit alias) to the fully-qualified class name.
    ///
    /// This is used to resolve simple type names found in PHPDoc `@return` tags,
    /// e.g. `Device` → `Netatmo\Models\Devices\Device`.
    fn build_use_map_from_statements<'arena>(
        &self,
        statements: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Statement<'arena>>,
    ) -> HashMap<String, String> {
        use mago_syntax::ast::{NamespaceBody, Statement, UseItems};

        let mut map = HashMap::new();

        for stmt in statements.iter() {
            match stmt {
                Statement::Namespace(ns) => {
                    let inner = match &ns.body {
                        NamespaceBody::Implicit(body) => &body.statements,
                        NamespaceBody::BraceDelimited(block) => &block.statements,
                    };
                    let inner_map = self.build_use_map_from_statements(inner);
                    map.extend(inner_map);
                }
                Statement::Use(use_stmt) => {
                    let items: Vec<_> = match &use_stmt.items {
                        UseItems::Sequence(seq) => seq.items.iter().collect(),
                        UseItems::TypedSequence(seq) => seq.items.iter().collect(),
                        UseItems::TypedList(list) => list.items.iter().collect(),
                        UseItems::MixedList(_) => vec![],
                    };
                    for item in items {
                        let fqn = item.name.value();
                        let alias = item
                            .alias
                            .as_ref()
                            .map(|a| a.identifier.value.to_string())
                            .unwrap_or_else(|| item.name.last_segment().to_string());
                        map.insert(alias, fqn.trim_start_matches('\\').replace('\\', "\\"));
                    }
                }
                _ => {}
            }
        }

        map
    }

    /// Walk statements looking for class/trait definitions and extract their
    /// typed property declarations and method return types.
    fn collect_property_types_from_statements<'arena>(
        &self,
        statements: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Statement<'arena>>,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        source: &str,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        use_map: &HashMap<String, String>,
    ) {
        use mago_syntax::ast::{NamespaceBody, Statement};

        for stmt in statements.iter() {
            match stmt {
                Statement::Namespace(ns) => match &ns.body {
                    NamespaceBody::Implicit(body) => {
                        // Build a namespace-scoped use map so aliases from one namespace
                        // do not leak into another.
                        let ns_use_map =
                            self.build_use_map_from_statements(&body.statements);
                        self.collect_property_types_from_statements(
                            &body.statements,
                            trivia,
                            source,
                            resolved_names,
                            &ns_use_map,
                        );
                    }
                    NamespaceBody::BraceDelimited(block) => {
                        let ns_use_map =
                            self.build_use_map_from_statements(&block.statements);
                        self.collect_property_types_from_statements(
                            &block.statements,
                            trivia,
                            source,
                            resolved_names,
                            &ns_use_map,
                        );
                    }
                },
                Statement::Class(class) => {
                    let class_fqn = self
                        .resolve_name(&class.name, resolved_names)
                        .unwrap_or_else(|| class.name.value.to_string());
                    self.collect_property_types_from_class_members(
                        &class_fqn,
                        class.members.iter(),
                        trivia,
                        source,
                        resolved_names,
                        use_map,
                    );
                }
                Statement::Trait(trait_def) => {
                    // Traits may also declare typed properties.
                    let trait_fqn = self
                        .resolve_name(&trait_def.name, resolved_names)
                        .unwrap_or_else(|| trait_def.name.value.to_string());
                    self.collect_property_types_from_class_members(
                        &trait_fqn,
                        trait_def.members.iter(),
                        trivia,
                        source,
                        resolved_names,
                        use_map,
                    );
                }
                Statement::Interface(iface) => {
                    let iface_fqn = self
                        .resolve_name(&iface.name, resolved_names)
                        .unwrap_or_else(|| iface.name.value.to_string());
                    self.collect_property_types_from_class_members(
                        &iface_fqn,
                        iface.members.iter(),
                        trivia,
                        source,
                        resolved_names,
                        use_map,
                    );
                }
                Statement::Enum(enum_def) => {
                    let enum_fqn = self
                        .resolve_name(&enum_def.name, resolved_names)
                        .unwrap_or_else(|| enum_def.name.value.to_string());
                    self.collect_property_types_from_class_members(
                        &enum_fqn,
                        enum_def.members.iter(),
                        trivia,
                        source,
                        resolved_names,
                        use_map,
                    );
                }
                _ => {}
            }
        }
    }

    /// Resolve a PHPDoc type-name string to a fully-qualified class name.
    ///
    /// Handles:
    /// - `\App\Models\User` (FQN with leading backslash) → strip `\`
    /// - `User` (simple name) → look up in `use_map`
    /// - built-in types → `None`
    fn resolve_phpdoc_type(
        &self,
        type_name: &str,
        use_map: &HashMap<String, String>,
        namespace: Option<&str>,
    ) -> Option<String> {
        // Strip leading backslash for FQNs
        let name = type_name.trim_start_matches('\\');

        // Reject built-in / scalar types
        let first_segment = name.split('\\').next().unwrap_or(name);
        if is_builtin_type(first_segment) {
            return None;
        }

        // Always try to resolve the FIRST segment against the use map, regardless
        // of whether the name is simple (`Device`) or qualified (`Devices\Device`).
        //
        // Example: `@return Devices\Device` in a file with `use Netatmo\Models\Devices`
        //   first_segment = "Devices"
        //   use_map["Devices"] = "Netatmo\Models\Devices"
        //   remaining     = "\Device"
        //   result        = "Netatmo\Models\Devices\Device"
        if let Some(fqn_prefix) = use_map.get(first_segment) {
            let remaining = &name[first_segment.len()..]; // starts with `\` when qualified
            if remaining.is_empty() {
                Some(fqn_prefix.clone())
            } else {
                Some(format!(
                    "{}\\{}",
                    fqn_prefix,
                    remaining.trim_start_matches('\\')
                ))
            }
        } else if name.contains('\\') {
            // Qualified but first segment not in use map — return as-is (already an FQN
            // relative to the current namespace; may not be perfect but is a best-effort).
            Some(name.to_string())
        } else {
            // Simple unqualified name not in use map — qualify with namespace if available.
            if let Some(ns) = namespace {
                Some(format!("{}\\{}", ns, name))
            } else {
                Some(name.to_string())
            }
        }
    }

    /// Extract typed property declarations and method return types from a class-like
    /// body and store them in `self.property_types` / `self.method_return_types`.
    fn collect_property_types_from_class_members<'arena, I>(
        &self,
        class_fqn: &str,
        members: I,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        source: &str,
        resolved_names: &mago_names::ResolvedNames<'arena>,
        use_map: &HashMap<String, String>,
    ) where
        I: Iterator<Item = &'arena mago_syntax::ast::ClassLikeMember<'arena>>,
    {
        use mago_span::HasSpan;
        use mago_syntax::ast::ClassLikeMember;

        for member in members {
            match member {
                ClassLikeMember::Property(prop) => {
                    if let Some(hint) = prop.hint() {
                        if let Some(type_fqn) = self.resolve_hint_to_fqn(hint, resolved_names, Some(class_fqn)) {
                            for var in prop.variables() {
                                let prop_name =
                                    var.name.strip_prefix('$').unwrap_or(var.name).to_string();
                                self.property_types
                                    .borrow_mut()
                                    .entry(class_fqn.to_string())
                                    .or_default()
                                    .insert(prop_name, type_fqn.clone());
                            }
                        }
                    }
                }
                ClassLikeMember::Method(method) => {
                    let method_name = method.name.value.to_string();

                    // 1. Try native PHP return type hint first (most reliable).
                    let return_fqn = method
                        .return_type_hint
                        .as_ref()
                        .and_then(|rt| self.resolve_hint_to_fqn(&rt.hint, resolved_names, Some(class_fqn)))
                        // 2. Fall back to PHPDoc `@return` tag.
                        .or_else(|| {
                            self.extract_phpdoc_return_type(
                                trivia,
                                method.span().start.offset,
                                source,
                                use_map,
                                Some(class_fqn),
                            )
                        });

                    if let Some(type_fqn) = return_fqn {
                        self.method_return_types
                            .borrow_mut()
                            .entry(class_fqn.to_string())
                            .or_default()
                            .insert(method_name, type_fqn);
                    }
                }
                _ => {}
            }
        }
    }

    /// Extract the return type from the PHPDoc block immediately preceding the
    /// definition at `def_offset`.  Returns `None` if no `@return` tag is found
    /// or if the type cannot be resolved to a class.
    fn extract_phpdoc_return_type<'arena>(
        &self,
        trivia: &mago_syntax::ast::Sequence<'arena, mago_syntax::ast::Trivia<'arena>>,
        def_offset: u32,
        source: &str,
        use_map: &HashMap<String, String>,
        class_fqn: Option<&str>,
    ) -> Option<String> {
        use mago_syntax::ast::TriviaKind;

        // Find the closest docblock that ends before def_offset.
        let mut best: Option<&mago_syntax::ast::Trivia<'arena>> = None;
        for t in trivia.iter() {
            if t.kind != TriviaKind::DocBlockComment {
                continue;
            }
            if t.span.end.offset <= def_offset {
                match best {
                    Some(prev) if t.span.end.offset > prev.span.end.offset => {
                        best = Some(t);
                    }
                    None => {
                        best = Some(t);
                    }
                    _ => {}
                }
            }
        }

        let docblock_trivia = best?;

        // Ensure the docblock is directly attached (no intervening statements).
        let docblock_end = docblock_trivia.span.end.offset;
        let between = &source[docblock_end as usize..def_offset as usize];
        if between.contains('{') || between.contains('}') || between.contains(';') {
            return None;
        }

        let arena = bumpalo::Bump::new();
        let doc = mago_docblock::parse_trivia(&arena, docblock_trivia).ok()?;

        // Find the first `@return` tag and extract its type name.
        for element in doc.elements.iter() {
            if let mago_docblock::document::Element::Tag(tag) = element {
                if tag.name.eq_ignore_ascii_case("return") {
                    // The description starts with the type, possibly followed by a
                    // variable name or further description: `@return Device $device desc`
                    let raw_type = tag
                        .description
                        .split_whitespace()
                        .next()
                        .unwrap_or("");

                    // Split union types on '|', strip leading/trailing '?', skip "null".
                    let type_name: Option<&str> = raw_type
                        .split('|')
                        .map(|s| s.trim().trim_matches('?'))
                        .find(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
                        .or_else(|| {
                            // Fallback: use first member even if it's null-like.
                            raw_type
                                .split('|')
                                .next()
                                .map(|s| s.trim().trim_matches('?'))
                                .filter(|s| !s.is_empty())
                        });

                    if let Some(type_name) = type_name {
                        if matches!(type_name.to_lowercase().as_str(), "self" | "static") {
                            return class_fqn.map(String::from);
                        }
                        let namespace = class_fqn
                            .and_then(|fqn| fqn.rfind('\\').map(|i| &fqn[..i]));
                        return self.resolve_phpdoc_type(type_name, use_map, namespace);
                    }
                }
            }
        }

        None
    }
}

/// Check if a type name is a PHP built-in type.
fn is_builtin_type(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "int"
            | "float"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "null"
            | "void"
            | "never"
            | "mixed"
            | "callable"
            | "iterable"
            | "parent"
            | "true"
            | "false"
            | "resource"
    )
}

/// Check if a constant name is a PHP built-in constant.
fn is_builtin_constant(name: &str) -> bool {
    matches!(
        name,
        "true"
            | "false"
            | "null"
            | "TRUE"
            | "FALSE"
            | "NULL"
            | "PHP_EOL"
            | "PHP_INT_MAX"
            | "PHP_INT_MIN"
            | "PHP_INT_SIZE"
            | "PHP_FLOAT_MAX"
            | "PHP_FLOAT_MIN"
            | "PHP_FLOAT_EPSILON"
            | "PHP_FLOAT_DIG"
            | "PHP_MAJOR_VERSION"
            | "PHP_MINOR_VERSION"
            | "PHP_RELEASE_VERSION"
            | "PHP_VERSION"
            | "PHP_VERSION_ID"
            | "PHP_OS"
            | "PHP_OS_FAMILY"
            | "PHP_SAPI"
            | "PHP_MAXPATHLEN"
            | "PHP_PREFIX"
            | "STDIN"
            | "STDOUT"
            | "STDERR"
            | "DIRECTORY_SEPARATOR"
            | "PATH_SEPARATOR"
    )
}
