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


; JavaScript for workflow scripting (inline)
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_uses (#eq? @_uses "uses")
    value: (flow_node) @_actions_ghs (#match? @_actions_ghs "^actions/github-script"))
  (block_mapping_pair
    key: (flow_node) @_with (#eq? @_with "with")
    value: (block_node
             (block_mapping
               (block_mapping_pair
                 key: (flow_node) @_run (#eq? @_run "script")
                 value: (flow_node
                          (plain_scalar
                            (string_scalar) @injection.content
                            (#set! injection.language "javascript"))))))))

; JavaScript for workflow scripting (block)
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_uses (#eq? @_uses "uses")
    value: (flow_node) @_actions_ghs (#match? @_actions_ghs "^actions/github-script"))
  (block_mapping_pair
    key: (flow_node) @_with (#any-of? @_with "with")
    value: (block_node
             (block_mapping
               (block_mapping_pair
                 key: (flow_node) @_run (#any-of? @_run "script")
                 value: (block_node
                          (block_scalar) @injection.content
                          (#set! injection.language "javascript")))))))