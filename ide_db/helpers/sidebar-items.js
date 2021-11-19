initSidebarItems({"fn":[["for_each_break_expr","Calls `cb` on each break expr inside of `body` that is applicable for the given label."],["for_each_tail_expr","Calls `cb` on each expression inside `expr` that is at “tail position”. Does not walk into `break` or `return` expressions. Note that modifying the tree while iterating it will cause undefined iteration which might potentially results in an out of bounds panic."],["get_path_in_derive_attr","Parses and returns the derive path at the cursor position in the given attribute, if it is a derive. This special case is required because the derive macro is a compiler builtin that discards the input derives."],["item_name",""],["mod_path_to_ast","Converts the mod path struct into its ast representation."],["pick_best_token","Picks the token with the highest rank returned by the passed in function."],["try_resolve_derive_input","Parses and resolves the path at the cursor position in the given attribute, if it is a derive. This special case is required because the derive macro is a compiler builtin that discards the input derives."],["visit_file_defs","Iterates all `ModuleDef`s and `Impl` blocks of the given file."]],"mod":[["famous_defs","See [`FamousDefs`]."],["generated_lints","Generated by `sourcegen_lints`, do not edit by hand."],["import_assets","Look up accessible paths for items."],["insert_use","Handle syntactic aspects of inserting a new `use` item."],["merge_imports","Handle syntactic aspects of merging UseTrees."],["node_ext","Various helper functions to work with SyntaxNodes."],["rust_doc","Rustdoc specific doc comment handling"]],"struct":[["SnippetCap",""]]});