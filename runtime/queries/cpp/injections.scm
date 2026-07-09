((comment) @injection.content
 (#set! injection.language "comment"))

((preproc_arg) @injection.content
 (#set! injection.language "c")
 (#set! injection.include-children))


((preproc_arg) @injection.content
 (#set! injection.language "cpp")
 (#set! injection.include-children))

(raw_string_literal
  delimiter: (raw_string_delimiter) @injection.language
  (raw_string_content) @injection.content)