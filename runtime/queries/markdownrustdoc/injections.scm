; From nvim-treesitter/nvim-treesitter

(fenced_code_block
  (code_fence_content) @injection.shebang @injection.content
  (#set! injection.include-unnamed-children))

(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content (#set! injection.include-unnamed-children))

((html_block) @injection.content
 (#set! injection.language "html")
 (#set! injection.include-unnamed-children)
 (#set! injection.combined))

((pipe_table_cell) @injection.content (#set! injection.language "markdown.inline") (#set! injection.include-unnamed-children))

((minus_metadata) @injection.content (#set! injection.language "yaml") (#set! injection.include-unnamed-children))
((plus_metadata) @injection.content (#set! injection.language "toml") (#set! injection.include-unnamed-children))

((inline) @injection.content (#set! injection.language "markdown.inline") (#set! injection.include-unnamed-children))


; In Rust, it is common to have documentation code blocks not specify the
; language, and it is assumed to be Rust if it is not specified.

(fenced_code_block
  (code_fence_content) @injection.content
  (#set! injection.language "rust")
  (#set! injection.include-unnamed-children))

(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content (#set! injection.include-unnamed-children))

(fenced_code_block
  (info_string
    (language) @__language)
  (code_fence_content) @injection.content
  ; list of attributes for Rust syntax highlighting:
  ; https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html#attributes
  (#match? @__language
  "(ignore|should_panic|no_run|compile_fail|standalone_crate|custom|edition*)")
  (#set! injection.language "rust")
  (#set! injection.include-unnamed-children))