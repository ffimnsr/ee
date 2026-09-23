; Fold yaml blocks by indentation structure.
;
; - (block_mapping_pair): folds the value block beneath a key; nested pairs
;   produce their own marker on their header line.
; - (block_sequence): folds the remaining items of a list.
; - (block_scalar): folds the literal/folded block content (`|` / `>`).
;
; Single-line nodes never yield a range: the fold engine only emits folds
; whose body spans past the header line, so `a: b` pairs and one-line
; sequence items stay unfoldable.

(block_mapping_pair) @fold
(block_sequence) @fold
(block_scalar) @fold