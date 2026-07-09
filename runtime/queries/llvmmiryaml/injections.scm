(block_mapping_pair
  key: (_) @yaml-key
  value: (block_node
    (block_scalar) @injection.content)
  (#eq? @yaml-key "commands")
  (#set! injection.language "bash"))

(block_mapping_pair
  key: (_) @yaml-key
  value: (block_node
    (block_sequence
      (block_sequence_item
        (block_node
          (block_scalar) @injection.content))))
  (#eq? @yaml-key "commands")
  (#set! injection.language "bash"))


((document (block_node (block_scalar) @injection.content))
 (#set! injection.language "llvm"))

((document (block_node (block_mapping (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar))) ; "body"
  value: (block_node (block_scalar) @injection.content)))))
  (#set! injection.language "mir"))