(function_declaration
  body: (block)? @function.inside) @function.around

(func_literal
  body: (block)? @function.inside) @function.around

(method_declaration
  body: (block)? @function.inside) @function.around

;; struct and interface declaration as class textobject?
(type_declaration
  (type_spec (type_identifier) (struct_type (field_declaration_list (_)?) @class.inside))) @class.around

(type_declaration
  (type_spec (type_identifier) (interface_type (method_elem)+ @class.inside))) @class.around

(type_parameter_list
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(parameter_list
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(argument_list
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(comment) @comment.inside

(comment)+ @comment.around

; Go test/benchmark/example/fuzz functions (all live in _test.go).
((function_declaration
   name: (identifier) @_name
   body: (block)? @test.inside) @test.around
 (#match? @_name "^(Test|Benchmark|Example|Fuzz)"))


(style_element) @xml-element.around
(style_element
  [(self_closing_style_tag) (style_element_text)] @xml-element.inside)

(script_element) @xml-element.around
(script_element
  [(script_element_text) (self_closing_script_tag)] @xml-element.inside)

(element) @xml-element.around
(element (self_closing_tag) @xml-element.inside)
(element (tag_start) (_)* @xml-element.inside (tag_end))

(element_comment) @comment.around

(component_declaration
  (component_block) @function.inside) @function.around

; TODO: function.inside textobjects
(css_declaration) @function.around

(script_declaration
  (script_block) @function.inside) @function.around