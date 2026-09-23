; Python newline indentation signals.
; Conservative subset of ee's indent contract. Supported captures: @indent,
; @indent.always (same opening-line behavior as @indent), @dedent and its
; Helix spellings @outdent/@outdent.always, @extend and @extend.prevent-once
; (no-ops in ee's caret-gated engine), @align + @anchor (absolute column
; alignment), and @opaque (lines strictly inside the node keep their indent).
;
; Grammar-error recovery rules (Helix-sourced) are supported: while the tree
; is broken mid-edit, the (ERROR ...) rules below keep newlines indenting for
; typical incomplete constructs. `#set! scope ...` settings are ignored by ee
; (no scope distinction in the delta model).

[
  (list)
  (tuple)
  (dictionary)
  (set)
  (if_statement)
  (for_statement)
  (while_statement)
  (with_statement)
  (try_statement)
  (match_statement)
  (case_clause)
  (import_from_statement)
  (parenthesized_expression)
  (generator_expression)
  (list_comprehension)
  (set_comprehension)
  (dictionary_comprehension)
  (tuple_pattern)
  (list_pattern)
  (argument_list)
  (parameters)
  (binary_operator)
  (function_definition)
  (class_definition)
] @indent

[
  ")"
  "]"
  "}"
] @dedent

(elif_clause "elif" @dedent)
(else_clause "else" @dedent)
(except_clause "except" @dedent)
(finally_clause "finally" @dedent)

; Grammar-error recovery rules (Helix-sourced). The python grammar folds many
; incomplete constructs into a single (ERROR) node; keep newlines indenting
; while the user is mid-typing.
(ERROR
  "try"
  .
  ":" @indent @extend)
(ERROR
  .
  "def") @indent @extend
(ERROR
  (block) @indent @extend
  (#set! "scope" "all"))
