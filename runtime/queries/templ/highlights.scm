
; Identifiers

(field_identifier) @variable.other.member

(identifier) @variable

(package_identifier) @namespace

(const_spec
  name: (identifier) @constant)

(keyed_element . (literal_element (identifier) @variable.other.member))
(field_declaration
  name: (field_identifier) @variable.other.member)

(parameter_declaration (identifier) @variable.parameter)
(variadic_parameter_declaration (identifier) @variable.parameter)

(label_name) @label

(const_spec
  name: (identifier) @constant)

; Function calls

(call_expression
  function: (identifier) @function)

(call_expression
  function: (selector_expression
    field: (field_identifier) @function.method))

(call_expression
  function: (identifier) @function.builtin
  (#match? @function.builtin "^(append|cap|close|complex|copy|delete|imag|len|make|new|panic|print|println|real|recover|min|max|clear)$"))

; Types

(type_identifier) @type

(type_parameter_list
  (type_parameter_declaration
    name: (identifier) @type.parameter))

((type_identifier) @type.builtin
  (#match? @type.builtin "^(any|bool|byte|comparable|complex128|complex64|error|float32|float64|int|int16|int32|int64|int8|rune|string|uint|uint16|uint32|uint64|uint8|uintptr)$"))

; Type definition names: `type Foo struct{}`, `type Bar = Baz`.
(type_spec
  name: (type_identifier) @type.definition)
(type_alias
  name: (type_identifier) @type.definition)

; Function definitions

(function_declaration
  name: (identifier) @function)

(method_declaration
  name: (field_identifier) @function.method)

(method_elem
  name: (field_identifier) @function.method)

; Blank identifier `_` (Go's discard) — dim as unused.
; It parses as (blank_identifier) in imports and as (identifier) elsewhere
; (`_ = x`, `a, _ := f()`, `for _, v := range`).
(blank_identifier) @comment.unused
((identifier) @comment.unused
 (#eq? @comment.unused "_"))

; Operators

[
  "--"
  "-"
  "-="
  ":="
  "!"
  "!="
  "..."
  "*"
  "*"
  "*="
  "/"
  "/="
  "&"
  "&&"
  "&="
  "%"
  "%="
  "^"
  "^="
  "+"
  "++"
  "+="
  "<-"
  "<"
  "<<"
  "<<="
  "<="
  "="
  "=="
  ">"
  ">="
  ">>"
  ">>="
  "|"
  "|="
  "||"
  "&^"
  "&^="
  "~"
] @operator

; Keywords

[
  "default"
  "type"
] @keyword

[
  "defer"
  "go"
  "goto"
] @keyword.control

[
  "if"
  "else"
  "switch"
  "select"
  "case"
] @keyword.control.conditional

[
  "for"
  "range"
] @keyword.control.repeat

[
  "import"
  "package"
] @keyword.control.import

[
  "return"
  "continue"
  "break"
  "fallthrough"
] @keyword.control.return

[
  "func"
] @keyword.function

[
  "var"
  "chan"
  "interface"
  "map"
  "struct"
] @keyword.storage.type

[
  "const"
] @keyword.storage.modifier

; Delimiters

[
  ":"
  "."
  ","
  ";"
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

; Literals

[
  (interpreted_string_literal)
  (raw_string_literal)
] @string

(rune_literal) @constant.character

(escape_sequence) @constant.character.escape

[
  (int_literal)
] @constant.numeric.integer

[
  (float_literal)
  (imaginary_literal)
] @constant.numeric.float

[
  (true)
  (false)
] @constant.builtin.boolean

[
  (nil)
  (iota)
] @constant.builtin

; Comments
(comment) @comment

; Doc Comments
(source_file
  (comment) @comment.block.documentation . (comment)* . [
    (package_clause) ; `package`
    (type_declaration) ; `type`
    (function_declaration) ; `func`
    (method_declaration) ; `func`
    (var_declaration) ; `var`
    (const_declaration) ; `const`
    ; var (
    ; 	A = 1
    ; 	B = 2
    ; )
    (var_spec)
    ; const (
    ; 	A = 1
    ; 	B = 2
    ; )
    (const_spec)
  ])


(css_declaration
  name: (css_identifier) @function)
(script_declaration
  name: (script_identifier) @function)
(component_declaration
  name: (component_identifier) @function)

; Elements

(tag_start name: (_) @tag)
(tag_end name: (_) @tag)
(self_closing_tag name: (_) @tag)

(tag_start ["<" ">"] @punctuation.bracket)
(tag_end ["</" ">"] @punctuation.bracket)
(self_closing_tag ["<" "/>"] @punctuation.bracket)

(style_tag_start "style" @tag)
(style_tag_end "style" @tag)
(self_closing_style_tag "style" @tag)

(style_tag_start ["<" ">"] @punctuation.bracket)
(style_tag_end ["</" ">"] @punctuation.bracket)
(self_closing_style_tag ["<" "/>"] @punctuation.bracket)

(script_tag_start "script" @tag)
(script_tag_end "script" @tag)
(self_closing_script_tag "script" @tag)

(script_tag_start ["<" ">"] @punctuation.bracket)
(script_tag_end ["</" ">"] @punctuation.bracket)
(self_closing_script_tag ["<" "/>"] @punctuation.bracket)

; Attributes

(attribute
  name: (attribute_name) @attribute)
(attribute
  value: (quoted_attribute_value) @string)

(css_property
  name: (css_property_name) @variable.other.member)
(css_property
  value: (css_property_value) @constant)

(dynamic_class_attribute_value) @function.method

; Special Elements and Attributes

((attribute
  name: (attribute_name) @attribute
  value: (quoted_attribute_value (attribute_value) @markup.link.url))
 (#any-of? @attribute "href" "src"))

((element
  (tag_start
    name: (_) @tag)
  (element_text) @markup.link.label)
  (#eq? @tag "a"))

((element
  (tag_start
    name: (_) @tag)
  (element_text) @markup.bold)
  (#any-of? @tag "strong" "b"))

((element
  (tag_start
    name: (_) @tag)
  (element_text) @markup.italic)
  (#any-of? @tag "em" "i"))

((element
  (tag_start
    name: (_) @tag)
  (element_text) @markup.strikethrough)
  (#any-of? @tag "s" "del"))

; Extra Features

(component_import
  name: (component_identifier) @function)

(component_render) @function

"@" @operator

[
  "templ"
  "css"
  "type"
] @keyword.storage.type
(script_declaration "script" @keyword.storage.type)

["{{" "}}"] @punctuation.bracket

; Comments

(element_comment) @comment