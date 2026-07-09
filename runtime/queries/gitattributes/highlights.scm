(pattern_char) @string
(pattern_char_escaped) @constant.character.escape

(directory_separator) @string
(directory_separator_escaped) @constant.character.escape

(negation) @operator

(wildcard_char_single) @operator
(wildcard_chars) @operator
(wildcard_chars_allow_slash) @operator

(bracket_char) @string
(bracket_char_escaped) @constant.character.escape

(bracket_negation) @operator
(bracket_range) @operator
(bracket_char_class) @keyword

[
  "["
  "]"
] @punctuation.bracket

(comment) @comment


(attribute) @variable
(value) @string

(quoted_pattern ["\""] @string)

(attribute_unset) @operator
(attribute_set_to) @operator

; Highlight builtin diff configuration
; The list of languages is taken from here https://git-scm.com/docs/gitattributes#_defining_a_custom_hunk_header
(attribute_set_to
  (attribute) @operator (#eq? @operator "diff")
  (value) @variable.builtin (#any-of? @variable.builtin
    "ada"
    "bash"
    "bibtex"
    "cpp"
    "csharp"
    "css"
    "dts"
    "elixir"
    "fortran"
    "fountain"
    "golang"
    "html"
    "java"
    "kotlin"
    "markdown"
    "matlab"
    "objc"
    "pascal"
    "perl"
    "php"
    "python"
    "ruby"
    "rust"
    "scheme"
    "tex"
  ))