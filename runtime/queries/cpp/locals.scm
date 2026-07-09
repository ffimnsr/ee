;; Scopes
(function_definition) @local.scope
(declaration) @local.scope

;; Definitions

; Parameters
; Up to 6 layers of declarators
(parameter_declaration
  (identifier) @local.definition.variable.parameter)
(parameter_declaration
  (_
    (identifier) @local.definition.variable.parameter))
(parameter_declaration
  (_
    (_
      (identifier) @local.definition.variable.parameter)))
(parameter_declaration
  (_
    (_
      (_
        (identifier) @local.definition.variable.parameter))))
(parameter_declaration
  (_
    (_
      (_
        (_
          (identifier) @local.definition.variable.parameter)))))
(parameter_declaration
  (_
    (_
      (_
        (_
          (_
            (identifier) @local.definition.variable.parameter))))))

;; References

(identifier) @local.reference

; A call's function name is not a variable reference; keep its class
; even when a same-named local is in scope.
(call_expression
  function: (identifier) @_)


; C++-specific scopes on top of c's function_definition / declaration scopes.
[
  (lambda_expression)
  (namespace_definition)
  (class_specifier)
  (for_range_loop)
] @local.scope

; C++-only parameter forms (c only has parameter_declaration).
(optional_parameter_declaration
  declarator: (identifier) @local.definition.variable.parameter)
(variadic_parameter_declaration
  declarator: (variadic_declarator (identifier) @local.definition.variable.parameter))

; Template type parameters.
(type_parameter_declaration
  (type_identifier) @local.definition.type)