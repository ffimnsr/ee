; JSON newline indentation signals.
; Conservative Helix-inspired subset adapted to ee's current MVP contract.
; Supported captures today:
; - @indent
; - @dedent

[
  (object)
  (array)
] @indent

[
  "]"
  "}"
] @dedent
