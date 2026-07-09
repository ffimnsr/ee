; aligns forms to the second position if there's two in a line:
; (-> 10
;     (* 2)
;     (print))

  ; anything that doesn't match should be indented normally
  ; from https://github.com/janet-lang/spork/blob/5601dc883535473bca28351cc6df04ed6c656c65/spork/fmt.janet#L87C12-L93C38

; everything else should be indented normally:
;
; (let [foo 10]
;   (print foo))
;
; (foo
;   bar)
(par_tup_lit . (sym_lit)) @indent

; for `{}` and `[]`:
; {:foo 10
;  :bar 20}

; [foo
;  bar]
