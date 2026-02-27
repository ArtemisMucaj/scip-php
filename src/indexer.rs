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
}

impl Indexer {
    pub fn new(project: PhpProject) -> Self {
        Indexer {
            project,
            var_types: RefCell::new(HashMap::new()),
        }
    }

    /// Run the indexer and produce a SCIP Index.
    pub fn index(&self) -> Result<Index> {
        let php_files = self.project.discover_php_files();
        eprintln!("scip-php: found {} PHP files", php_files.len());

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
            self.populate_var_types_from_params(&method.parameter_list, resolved_names);
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
        self.populate_var_types_from_params(&func.parameter_list, resolved_names);
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

            // Assignment: walk both sides, and infer variable types from `new ClassName()`
            Expression::Assignment(assign) => {
                // If LHS is a simple variable and RHS is `new ClassName(...)`, track the type
                if let Expression::Variable(mago_syntax::ast::Variable::Direct(dv)) = assign.lhs {
                    if let Expression::Instantiation(inst) = assign.rhs {
                        if let Some(class_fqn) = self.try_resolve_class_from_expr(
                            inst.class,
                            resolved_names,
                            enclosing_class_fqn,
                        ) {
                            self.var_types
                                .borrow_mut()
                                .insert(dv.name.to_string(), class_fqn);
                        }
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
        use mago_syntax::ast::Expression;
        use mago_syntax::ast::Variable;

        match expr {
            Expression::Variable(Variable::Direct(dv)) if dv.name == "$this" => {
                enclosing_class_fqn.map(|s| s.to_string())
            }
            Expression::Variable(Variable::Direct(dv)) => {
                // Look up variable type from parameter type hints
                self.var_types.borrow().get(dv.name).cloned()
            }
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
    ) -> Option<String> {
        use mago_syntax::ast::Hint;
        match hint {
            Hint::Identifier(ident) => {
                let name = ident.value();
                if is_builtin_type(name) {
                    None
                } else {
                    let fqn = self
                        .resolve_identifier(ident, resolved_names)
                        .unwrap_or_else(|| name.to_string());
                    Some(fqn)
                }
            }
            Hint::Nullable(nullable) => self.resolve_hint_to_fqn(&nullable.hint, resolved_names),
            // Union/intersection types are ambiguous — don't resolve
            _ => None,
        }
    }

    /// Populate `var_types` from parameter type hints of a function-like parameter list.
    fn populate_var_types_from_params<'arena>(
        &self,
        params: &mago_syntax::ast::FunctionLikeParameterList<'arena>,
        resolved_names: &mago_names::ResolvedNames<'arena>,
    ) {
        let mut var_types = self.var_types.borrow_mut();
        for param in params.parameters.iter() {
            if let Some(hint) = &param.hint {
                if let Some(fqn) = self.resolve_hint_to_fqn(hint, resolved_names) {
                    var_types.insert(param.variable.name.to_string(), fqn);
                }
            }
        }
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
            | "self"
            | "static"
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
