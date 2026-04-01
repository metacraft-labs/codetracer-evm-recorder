//! Parser for the Solidity AST produced by `solc --combined-json ast`.
//!
//! We extract function definitions and their local variable declarations so
//! that the stack tracker can match stack slots to variable names.
//!
//! Only **unoptimized** Solidity code is targeted.  With `--optimize` the
//! compiler rearranges the stack in ways that make a simple source-offset
//! heuristic unreliable.

use serde_json::Value;

/// A source range in the same coordinate system as Solidity source maps:
/// byte offset + byte length + file index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRange {
    /// Byte offset from the start of the source file.
    pub offset: i32,
    /// Length in bytes of the range.
    pub length: i32,
    /// Source file index (matches the `f` field in `srcmap-runtime`).
    pub file_index: i32,
}

impl SourceRange {
    /// Try to parse a `"s:l:f"` src string (e.g. `"100:20:0"`).
    pub fn parse(src: &str) -> Option<Self> {
        let parts: Vec<&str> = src.splitn(4, ':').collect();
        let offset = parts.first().and_then(|s| s.parse::<i32>().ok())?;
        let length = parts.get(1).and_then(|s| s.parse::<i32>().ok())?;
        let file_index = parts
            .get(2)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        Some(Self {
            offset,
            length,
            file_index,
        })
    }

    /// Returns `true` when `offset` falls within `[range.offset, range.offset + range.length)`.
    pub fn contains_offset(&self, offset: i32) -> bool {
        offset >= self.offset && offset < self.offset + self.length
    }
}

/// A local variable or parameter declared in a Solidity function.
#[derive(Debug, Clone)]
pub struct VarDecl {
    /// Variable name as written in the source.
    pub name: String,
    /// Solidity type name (e.g. `"uint256"`, `"address"`, `"bool"`).
    pub type_name: String,
    /// Source range of the *variable declaration* node (covers `uint256 a`).
    pub src: SourceRange,
    /// Byte offset in the source where this variable is first
    /// declared/assigned (same as `src.offset`).
    pub declaration_offset: i32,
    /// Source range of the enclosing *VariableDeclarationStatement* node
    /// (covers the whole `uint256 a = 10;`), if available.  This range is
    /// needed for stack-tracker labelling because solc's source map may
    /// point PUSH instructions to the initializer expression rather than
    /// the declaration itself.
    pub statement_range: Option<SourceRange>,
}

/// A function (or constructor / fallback) extracted from the AST.
#[derive(Debug, Clone)]
pub struct FunctionDef {
    /// Function name (`"constructor"`, `"fallback"`, `"receive"`, or the
    /// name as written in the source).
    pub name: String,
    /// Source range of the whole function definition.
    pub src: SourceRange,
    /// Formal parameters.
    pub parameters: Vec<VarDecl>,
    /// Local variables declared inside the function body.
    pub local_variables: Vec<VarDecl>,
}

impl FunctionDef {
    /// All variables visible at source byte `offset` — parameters plus locals
    /// whose declaration precedes `offset`.
    pub fn vars_in_scope_at(&self, offset: i32) -> Vec<&VarDecl> {
        let mut vars: Vec<&VarDecl> = Vec::new();
        // Parameters are always in scope inside the function.
        for p in &self.parameters {
            vars.push(p);
        }
        // Locals are in scope after their declaration offset.
        for lv in &self.local_variables {
            if lv.declaration_offset <= offset {
                vars.push(lv);
            }
        }
        vars
    }
}

/// The parsed Solidity AST for one compilation unit (one or more files).
#[derive(Debug, Default)]
pub struct SolidityAst {
    /// All function/constructor/fallback definitions found in the AST.
    pub functions: Vec<FunctionDef>,
}

impl SolidityAst {
    /// Parse the AST portion of `solc --combined-json ast` output.
    ///
    /// The expected top-level structure is:
    /// ```json
    /// {
    ///   "sources": {
    ///     "path/to/File.sol": {
    ///       "AST": { ... }
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// This function is tolerant of missing fields and silently skips nodes it
    /// cannot parse.
    pub fn from_combined_json(json: &str) -> eyre::Result<Self> {
        let root: Value = serde_json::from_str(json)?;
        let mut ast = SolidityAst::default();

        if let Some(sources) = root.get("sources").and_then(|v| v.as_object()) {
            for (_path, src_obj) in sources {
                if let Some(ast_node) = src_obj.get("AST") {
                    ast.visit_node(ast_node);
                }
            }
        }

        // Also handle a bare AST node (useful in tests).
        if root.get("nodeType").is_some() {
            ast.visit_node(&root);
        }

        Ok(ast)
    }

    /// Find the function whose source range contains `offset` in file
    /// `file_index`.
    pub fn function_at(&self, offset: i32, file_index: i32) -> Option<&FunctionDef> {
        self.functions
            .iter()
            .find(|f| f.src.file_index == file_index && f.src.contains_offset(offset))
    }

    // -----------------------------------------------------------------------
    // Internal recursive visitor
    // -----------------------------------------------------------------------

    fn visit_node(&mut self, node: &Value) {
        let node_type = match node.get("nodeType").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return,
        };

        match node_type {
            "SourceUnit" | "ContractDefinition" => {
                self.visit_children(node);
            }
            "FunctionDefinition" => {
                if let Some(fdef) = Self::parse_function(node) {
                    self.functions.push(fdef);
                }
                // Do NOT recurse further; parse_function already collects locals.
            }
            _ => {
                // Other top-level nodes (state-variable declarations, etc.):
                // recurse so we handle nested contracts.
                self.visit_children(node);
            }
        }
    }

    fn visit_children(&mut self, node: &Value) {
        // Try "nodes" (SourceUnit / ContractDefinition children)
        if let Some(nodes) = node.get("nodes").and_then(|v| v.as_array()) {
            for child in nodes {
                self.visit_node(child);
            }
        }
        // Try "body" statements (though we recurse into function body
        // separately via parse_function)
        if let Some(body) = node.get("body") {
            self.visit_node(body);
        }
        // Try "statements"
        if let Some(stmts) = node.get("statements").and_then(|v| v.as_array()) {
            for s in stmts {
                self.visit_node(s);
            }
        }
    }

    fn parse_function(node: &Value) -> Option<FunctionDef> {
        let src_str = node.get("src")?.as_str()?;
        let src = SourceRange::parse(src_str)?;

        let name = match node.get("kind").and_then(|v| v.as_str()) {
            Some("constructor") => "constructor".to_string(),
            Some("fallback") => "fallback".to_string(),
            Some("receive") => "receive".to_string(),
            _ => node
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string(),
        };

        // Input parameters
        let parameters = node
            .get("parameters")
            .and_then(|p| p.get("parameters"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(Self::parse_var_decl_node).collect())
            .unwrap_or_default();

        // Named return variables — these are implicitly declared locals
        // initialized to their type's zero value.  They are visible
        // throughout the function body, so we treat them like locals.
        let mut local_variables: Vec<VarDecl> = Vec::new();
        if let Some(ret_params) = node
            .get("returnParameters")
            .and_then(|p| p.get("parameters"))
            .and_then(|v| v.as_array())
        {
            for rp in ret_params {
                if let Some(v) = Self::parse_var_decl_node(rp) {
                    // Only add named return variables (unnamed have empty name).
                    if !v.name.is_empty() {
                        local_variables.push(v);
                    }
                }
            }
        }

        // Collect all VariableDeclarationStatement nodes from the body.
        if let Some(body) = node.get("body") {
            Self::collect_local_vars(body, &mut local_variables);
        }

        Some(FunctionDef {
            name,
            src,
            parameters,
            local_variables,
        })
    }

    /// Recursively collect VariableDeclarationStatement nodes.
    fn collect_local_vars(node: &Value, out: &mut Vec<VarDecl>) {
        let node_type = node.get("nodeType").and_then(|v| v.as_str()).unwrap_or("");

        if node_type == "VariableDeclarationStatement" {
            // Parse the statement-level source range (covers the whole
            // `uint256 x = expr;` including the initializer).
            let stmt_range = node
                .get("src")
                .and_then(|v| v.as_str())
                .and_then(SourceRange::parse);

            // "declarations" is an array; each element is either a
            // VariableDeclaration node or null (for tuple destructuring blanks).
            if let Some(decls) = node.get("declarations").and_then(|v| v.as_array()) {
                for decl in decls {
                    if decl.is_null() {
                        continue;
                    }
                    if let Some(mut v) = Self::parse_var_decl_node(decl) {
                        v.statement_range = stmt_range.clone();
                        out.push(v);
                    }
                }
            }
        }

        // Recurse into sub-nodes that may contain variable declarations.
        //
        // - statements:                Block, UncheckedBlock
        // - body:                      ForStatement, WhileStatement, DoWhileStatement
        // - trueBody / falseBody:      IfStatement
        // - initializationExpression:  ForStatement (e.g. `uint i = 0`)
        // - loopExpression:            ForStatement (e.g. `i++`)
        // - clauses:                   TryStatement (array of TryCatchClause)
        // - block:                     TryCatchClause body
        for key in &[
            "statements",
            "body",
            "trueBody",
            "falseBody",
            "initializationExpression",
            "loopExpression",
            "clauses",
            "block",
        ] {
            if let Some(child) = node.get(key) {
                if let Some(arr) = child.as_array() {
                    for item in arr {
                        Self::collect_local_vars(item, out);
                    }
                } else {
                    Self::collect_local_vars(child, out);
                }
            }
        }

        // TryCatchClause: extract the error parameters (e.g. `string memory reason`
        // in `catch Error(string memory reason) { ... }`).
        if node_type == "TryCatchClause" {
            if let Some(params) = node
                .get("parameters")
                .and_then(|p| p.get("parameters"))
                .and_then(|v| v.as_array())
            {
                for p in params {
                    if let Some(v) = Self::parse_var_decl_node(p) {
                        if !v.name.is_empty() {
                            out.push(v);
                        }
                    }
                }
            }
        }
    }

    /// Parse a single VariableDeclaration AST node into a `VarDecl`.
    fn parse_var_decl_node(node: &Value) -> Option<VarDecl> {
        let name = node.get("name")?.as_str()?.to_string();
        let src_str = node.get("src")?.as_str()?;
        let src = SourceRange::parse(src_str)?;
        let declaration_offset = src.offset;

        let type_name = node
            .get("typeName")
            .and_then(|tn| {
                // ElementaryTypeName → name field
                tn.get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    // UserDefinedTypeName → pathNode.name or name
                    .or_else(|| {
                        tn.get("pathNode")
                            .and_then(|pn| pn.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .or_else(|| tn.get("name").and_then(|v| v.as_str()).map(str::to_string))
            })
            .or_else(|| {
                // Sometimes the type info is in typeDescriptions
                node.get("typeDescriptions")
                    .and_then(|td| td.get("typeString"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "uint256".to_string());

        Some(VarDecl {
            name,
            type_name,
            src,
            declaration_offset,
            statement_range: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_range_parse() {
        let r = SourceRange::parse("100:20:0").unwrap();
        assert_eq!(r.offset, 100);
        assert_eq!(r.length, 20);
        assert_eq!(r.file_index, 0);

        assert!(r.contains_offset(100));
        assert!(r.contains_offset(110));
        assert!(!r.contains_offset(120));
        assert!(!r.contains_offset(99));
    }

    #[test]
    fn test_parse_minimal_ast() {
        // Minimal combined-json style AST for a function with two locals.
        let json = r#"{
            "sources": {
                "FlowTest.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "FlowTest",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "compute",
                                "kind": "function",
                                "src": "200:300:0",
                                "parameters": {
                                    "parameters": []
                                },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": [
                                        {
                                            "nodeType": "VariableDeclarationStatement",
                                            "src": "220:14:0",
                                            "declarations": [{
                                                "nodeType": "VariableDeclaration",
                                                "name": "a",
                                                "src": "220:9:0",
                                                "typeName": {
                                                    "nodeType": "ElementaryTypeName",
                                                    "name": "uint256"
                                                }
                                            }]
                                        },
                                        {
                                            "nodeType": "VariableDeclarationStatement",
                                            "src": "240:14:0",
                                            "declarations": [{
                                                "nodeType": "VariableDeclaration",
                                                "name": "b",
                                                "src": "240:9:0",
                                                "typeName": {
                                                    "nodeType": "ElementaryTypeName",
                                                    "name": "uint256"
                                                }
                                            }]
                                        }
                                    ]
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        assert_eq!(ast.functions.len(), 1);

        let f = &ast.functions[0];
        assert_eq!(f.name, "compute");
        assert_eq!(f.local_variables.len(), 2);
        assert_eq!(f.local_variables[0].name, "a");
        assert_eq!(f.local_variables[0].type_name, "uint256");
        assert_eq!(f.local_variables[1].name, "b");

        // Both locals should be in scope at offset 260 (after both declarations)
        let in_scope = f.vars_in_scope_at(260);
        assert_eq!(in_scope.len(), 2);

        // Only 'a' should be in scope at offset 230 (between the two decls)
        let in_scope_early = f.vars_in_scope_at(230);
        assert_eq!(in_scope_early.len(), 1);
        assert_eq!(in_scope_early[0].name, "a");
    }

    #[test]
    fn test_function_with_params() {
        let json = r#"{
            "sources": {
                "T.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "T",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "add",
                                "kind": "function",
                                "src": "0:60:0",
                                "parameters": {
                                    "parameters": [
                                        {
                                            "nodeType": "VariableDeclaration",
                                            "name": "x",
                                            "src": "10:9:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        },
                                        {
                                            "nodeType": "VariableDeclaration",
                                            "name": "y",
                                            "src": "21:9:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        }
                                    ]
                                },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": []
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        assert_eq!(ast.functions.len(), 1);
        let f = &ast.functions[0];
        assert_eq!(f.parameters.len(), 2);
        assert_eq!(f.parameters[0].name, "x");
        assert_eq!(f.parameters[1].name, "y");
        assert_eq!(f.local_variables.len(), 0);
        // Parameters don't come from VariableDeclarationStatement, so
        // statement_range should be None.
        assert!(f.parameters[0].statement_range.is_none());
    }

    #[test]
    fn test_statement_range_captured() {
        // Verify that local variables get their statement_range set from
        // the enclosing VariableDeclarationStatement's src.
        let json = r#"{
            "sources": {
                "S.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "S",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "f",
                                "kind": "function",
                                "src": "0:100:0",
                                "parameters": { "parameters": [] },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": [{
                                        "nodeType": "VariableDeclarationStatement",
                                        "src": "20:18:0",
                                        "declarations": [{
                                            "nodeType": "VariableDeclaration",
                                            "name": "x",
                                            "src": "20:9:0",
                                            "typeName": {
                                                "nodeType": "ElementaryTypeName",
                                                "name": "uint256"
                                            }
                                        }]
                                    }]
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        let f = &ast.functions[0];
        assert_eq!(f.local_variables.len(), 1);
        let var = &f.local_variables[0];
        assert_eq!(var.name, "x");
        // declaration src covers "uint256 x" (20:9)
        assert_eq!(var.src.offset, 20);
        assert_eq!(var.src.length, 9);
        // statement range covers "uint256 x = 42;" (20:18)
        let stmt = var
            .statement_range
            .as_ref()
            .expect("statement_range should be set");
        assert_eq!(stmt.offset, 20);
        assert_eq!(stmt.length, 18);
        // Offset 35 (within "= 42" part) is inside statement but outside decl
        assert!(!var.src.contains_offset(35));
        assert!(stmt.contains_offset(35));
    }

    #[test]
    fn test_named_return_variables() {
        let json = r#"{
            "sources": {
                "T.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "T",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "calc",
                                "kind": "function",
                                "src": "0:100:0",
                                "parameters": {
                                    "parameters": [{
                                        "nodeType": "VariableDeclaration",
                                        "name": "a",
                                        "src": "10:9:0",
                                        "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                    }]
                                },
                                "returnParameters": {
                                    "parameters": [
                                        {
                                            "nodeType": "VariableDeclaration",
                                            "name": "sum",
                                            "src": "30:9:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        },
                                        {
                                            "nodeType": "VariableDeclaration",
                                            "name": "",
                                            "src": "41:7:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        }
                                    ]
                                },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": []
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        let f = &ast.functions[0];
        assert_eq!(f.parameters.len(), 1);
        assert_eq!(f.parameters[0].name, "a");
        // Named return "sum" should appear as a local, but unnamed return should not.
        assert_eq!(f.local_variables.len(), 1);
        assert_eq!(f.local_variables[0].name, "sum");
    }

    #[test]
    fn test_for_loop_init_variable() {
        let json = r#"{
            "sources": {
                "T.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "T",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "loop",
                                "kind": "function",
                                "src": "0:200:0",
                                "parameters": { "parameters": [] },
                                "returnParameters": { "parameters": [] },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": [{
                                        "nodeType": "ForStatement",
                                        "src": "50:80:0",
                                        "initializationExpression": {
                                            "nodeType": "VariableDeclarationStatement",
                                            "src": "55:12:0",
                                            "declarations": [{
                                                "nodeType": "VariableDeclaration",
                                                "name": "i",
                                                "src": "55:9:0",
                                                "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                            }]
                                        },
                                        "body": {
                                            "nodeType": "Block",
                                            "statements": [{
                                                "nodeType": "VariableDeclarationStatement",
                                                "src": "100:18:0",
                                                "declarations": [{
                                                    "nodeType": "VariableDeclaration",
                                                    "name": "temp",
                                                    "src": "100:9:0",
                                                    "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                                }]
                                            }]
                                        }
                                    }]
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        let f = &ast.functions[0];
        let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();
        // Both the for-loop init var "i" and the body var "temp" should be collected.
        assert!(
            local_names.contains(&"i"),
            "for-loop init var 'i' should be collected; got {:?}",
            local_names
        );
        assert!(
            local_names.contains(&"temp"),
            "for-loop body var 'temp' should be collected; got {:?}",
            local_names
        );
    }

    #[test]
    fn test_unchecked_block_variables() {
        let json = r#"{
            "sources": {
                "T.sol": {
                    "AST": {
                        "nodeType": "SourceUnit",
                        "nodes": [{
                            "nodeType": "ContractDefinition",
                            "name": "T",
                            "nodes": [{
                                "nodeType": "FunctionDefinition",
                                "name": "f",
                                "kind": "function",
                                "src": "0:100:0",
                                "parameters": { "parameters": [] },
                                "returnParameters": { "parameters": [] },
                                "body": {
                                    "nodeType": "Block",
                                    "statements": [{
                                        "nodeType": "UncheckedBlock",
                                        "src": "20:50:0",
                                        "statements": [{
                                            "nodeType": "VariableDeclarationStatement",
                                            "src": "30:18:0",
                                            "declarations": [{
                                                "nodeType": "VariableDeclaration",
                                                "name": "x",
                                                "src": "30:9:0",
                                                "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                            }]
                                        }]
                                    }]
                                }
                            }]
                        }]
                    }
                }
            }
        }"#;

        let ast = SolidityAst::from_combined_json(json).unwrap();
        let f = &ast.functions[0];
        assert_eq!(f.local_variables.len(), 1);
        assert_eq!(f.local_variables[0].name, "x");
    }
}
