(identifier) @variable

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z\\d_]*$"))

[
  "sizeof"
  "offsetof"
  "alignof"
  "_Alignof"
  "asm"
  "__asm__"
] @keyword

[
  "enum"
  "struct"
  "typedef"
  "union"
] @keyword.storage.type

[
  (type_qualifier)
  (storage_class_specifier)
] @keyword.storage.modifier

[
  "goto"
  "break"
  "continue"
] @keyword.control

[
  "do"
  "for"
  "while"
] @keyword.control.repeat

[
  "if"
  "else"
  "switch"
  "case"
  "default"
] @keyword.control.conditional

"return" @keyword.control.return

[
  "defined"
  "#define"
  "#elif"
  "#else"
  "#endif"
  "#if"
  "#ifdef"
  "#ifndef"
  "#elifdef"
  "#elifndef"
  "#include"
  (preproc_directive)
] @keyword.directive

"..." @punctuation

["," "." ":" "::" ";" "->"] @punctuation.delimiter

["(" ")" "[" "]" "{" "}" "[[" "]]"] @punctuation.bracket

[
  "+"
  "-"
  "*"
  "/"
  "++"
  "--"
  "%"
  "=="
  "!="
  ">"
  "<"
  ">="
  "<="
  "&&"
  "||"
  "!"
  "&"
  "|"
  "^"
  "~"
  "<<"
  ">>"
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "<<="
  ">>="
  "&="
  "^="
  "|="
  "?"
] @operator

(conditional_expression ":" @operator) ; After punctuation

(pointer_declarator "*" @type.builtin) ; After Operators
(abstract_pointer_declarator "*" @type.builtin)


[(true) (false)] @constant.builtin.boolean

; C enumerators are integer constants; colour the definition the same as a use
; site (a SCREAMING_SNAKE reference resolves to @constant above), not as a
; distinct enum variant. C++ overrides this to keep @type.enum.variant.
(enumerator name: (identifier) @constant)

(string_literal) @string
(system_lib_string) @string

(null) @constant.builtin
(number_literal) @constant.numeric
(char_literal) @constant.character
(escape_sequence) @constant.character.escape

(field_identifier) @variable.other.member
(statement_identifier) @label
(type_identifier) @type
(primitive_type) @type.builtin
(sized_type_specifier) @type.builtin

; `typedef ... Name;` — the introduced name
(type_definition
  declarator: (type_identifier) @type.definition)

(call_expression
  function: (identifier) @function)
(call_expression
  function: (field_expression
    field: (field_identifier) @function))
(call_expression (argument_list (identifier) @variable))
(function_declarator
  declarator: [(identifier) (field_identifier)] @function)

; GCC builtins, e.g. __builtin_expect
((call_expression
  function: (identifier) @function.builtin)
 (#match? @function.builtin "^__builtin_"))

; Up to 6 layers of declarators
(parameter_declaration
  declarator: (identifier) @variable.parameter)
(parameter_declaration
  (_
    (identifier) @variable.parameter))
(parameter_declaration
  (_
    (_
      (identifier) @variable.parameter)))
(parameter_declaration
  (_
    (_
      (_
        (identifier) @variable.parameter))))
(parameter_declaration
  (_
    (_
      (_
        (_
          (identifier) @variable.parameter)))))
(parameter_declaration
  (_
    (_
      (_
        (_
          (_
            (identifier) @variable.parameter))))))

(preproc_function_def
  name: (identifier) @function.special)

(attribute
  name: (identifier) @attribute)

; GNU / MSVC attributes and calling conventions
[
  "__attribute__"
  "__declspec"
  "__based"
  "__cdecl"
  "__clrcall"
  "__stdcall"
  "__fastcall"
  "__thiscall"
  "__vectorcall"
] @attribute

; Builtin/predefined constants and macros.
((identifier) @constant.builtin
 (#any-of? @constant.builtin
   "stderr" "stdin" "stdout"
   "__FILE__" "__LINE__" "__DATE__" "__TIME__" "__func__"
   "__FUNCTION__" "__PRETTY_FUNCTION__" "__BASE_FILE__"
   "__STDC__" "__STDC_VERSION__" "__STDC_HOSTED__"
   "__VA_ARGS__" "__VA_OPT__" "__cplusplus"))

(comment) @comment


; cpp
((identifier) @variable.other.member
  (#match? @variable.other.member "^m_.*$"))

(parameter_declaration
  declarator: (reference_declarator) @variable.parameter)

; function(Foo ...foo)
(variadic_parameter_declaration
  declarator: (variadic_declarator
    (_) @variable.parameter))

; int foo = 0
(optional_parameter_declaration
  declarator: (_) @variable.parameter)

(field_declaration
  (field_identifier) @variable.other.member)

(field_initializer
  (field_identifier) @variable.other.member)

(function_declarator
  declarator: (field_identifier) @function.method)

(concept_definition
  name: (identifier) @type)

(alias_declaration
  name: (type_identifier) @type)

(namespace_identifier) @namespace

((namespace_identifier) @type
  (#match? @type "^[%u]"))

(case_statement
  value: (qualified_identifier
    (identifier) @constant))

(using_declaration
  .
  "using"
  .
  "namespace"
  .
  [
    (qualified_identifier)
    (identifier)
  ] @namespace)

(destructor_name
  (identifier) @function.method)

; functions
(function_declarator
  (qualified_identifier
    (identifier) @function))

(function_declarator
  (qualified_identifier
    (qualified_identifier
      (identifier) @function)))

(function_declarator
  (qualified_identifier
    (qualified_identifier
      (qualified_identifier
        (identifier) @function))))

(function_declarator
  (template_function
    (identifier) @function))

(operator_name) @function

"operator" @function

"static_assert" @function.builtin


(call_expression
  (qualified_identifier
    (identifier) @function))


(call_expression
  (qualified_identifier
    (qualified_identifier
      (identifier) @function)))

(call_expression
  (qualified_identifier
    (qualified_identifier
      (qualified_identifier
        (identifier) @function))))

(call_expression
  (template_function
    (identifier) @function))

(call_expression
  (qualified_identifier
    (template_function
      (identifier) @function)))

(call_expression
  (qualified_identifier
    (qualified_identifier
      (template_function
        (identifier) @function))))

(call_expression
  (qualified_identifier
    (qualified_identifier
      (qualified_identifier
        (template_function
          (identifier) @function)))))

; methods
(function_declarator
  (template_method
    (field_identifier) @function.method))

(call_expression
  (field_expression
    (field_identifier) @function.method))

; constructors
((function_declarator
  (qualified_identifier
    (identifier) @constructor))
  (#match? @constructor "^%u"))

((call_expression
  function: (identifier) @constructor)
  (#match? @constructor "^%u"))

((call_expression
  function: (qualified_identifier
    name: (identifier) @constructor))
  (#match? @constructor "^%u"))

((call_expression
  function: (field_expression
    field: (field_identifier) @constructor))
  (#match? @constructor "^%u"))

; constructing a type in an initializer list: Constructor ():  **SuperType (1)**
((field_initializer
  (field_identifier) @constructor
  (argument_list))
  (#match? @constructor "^%u"))

; Constants
(this) @variable.builtin

(null
  "nullptr" @constant.builtin)

(true) @constant.builtin.boolean

(false) @constant.builtin.boolean

; Literals
(raw_string_literal) @string

; Keywords
[
  "try"
  "catch"
  "noexcept"
  "throw"
] @keyword.control.exception

[
  "decltype"
  "explicit"
  "friend"
  "override"
  "using"
  "requires"
  "constexpr"
] @keyword

[
  "class"
  "namespace"
  "template"
  "typename"
  "concept"
] @keyword.storage.type

[
  "co_await"
  "co_yield"
  "co_return"
] @keyword

[
  "public"
  "private"
  "protected"
  "final"
  "virtual"
] @keyword.storage.modifier

[
  "new"
  "delete"
  "xor"
  "bitand"
  "bitor"
  "compl"
  "not"
  "xor_eq"
  "and_eq"
  "or_eq"
  "not_eq"
  "and"
  "or"
] @keyword.operator

"<=>" @operator

"::" @punctuation.delimiter

(template_argument_list
  [
    "<"
    ">"
  ] @punctuation.bracket)

(template_parameter_list
  [
    "<"
    ">"
  ] @punctuation.bracket)

(literal_suffix) @operator

; hlsl
[
  "in"
  "out"
  "inout"
  "uniform"
  "shared"
  "groupshared"
  "discard"
  "cbuffer"
  "row_major"
  "column_major"
  "globallycoherent"
  "centroid"
  "noperspective"
  "nointerpolation"
  "sample"
  "linear"
  "snorm"
  "unorm"
  "point"
  "line"
  "triangleadj"
  "lineadj"
  "triangle"
] @keyword

((identifier) @variable.builtin
  (#match? @variable.builtin "^SV_"))
; ((identifier) @variable)

(hlsl_attribute) @attribute

(hlsl_attribute
  [
    "["
    "]"
  ] @attribute)

"This" @type.builtin

[
  "interface"
  "extension"
  "property"
  "associatedtype"
  "where"
	"var"
	"let"
] @keyword

"__init" @constructor

[
  "__subscript"
  "get"
  "set"
] @function.builtin

(call_expression) @function

(call_expression (identifier)) @function

((call_expression
  function: (identifier) @function.builtin)
  (#any-of? @function.builtin
		"frac" "abs" "acos" "acosh" "asin" "asinh" "atan" "atanh" "cos" "cosh" "exp" "exp2" "floor" "log" "log10" "log2" "round" "rsqrt" "sin" "sincos" "sinh" "sqrt" "tan" "tanh" "trunc"
		"AllMemoryBarrier" "AllMemoryBarrierWithGroupSync" "DeviceMemoryBarrier" "DeviceMemoryBarrierWithGroupSync" "GroupMemoryBarrier" "GroupMemoryBarrierWithGroupSync"
		"abort" "clip" "errorf" "printf"
		"all" "any" "countbits" "faceforward" "firstbithigh" "firstbitlow" "isfinite" "isinf" "isnan" "max" "min" "noise" "pow" "reversebits" "sign"
		"asdouble" "asfloat" "asint" "asuint" "D3DCOLORtoUBYTE4" "f16tof32" "f32tof16"
		"ceil" "clamp" "degrees" "fma" "fmod" "frac" "frexp" "ldexp" "lerp" "mad" "modf" "radiants" "saturate" "smoothstep" "step"
		"cross" "determinant" "distance" "dot" "dst" "length" "lit" "msad4" "mul" "normalize" "rcp" "reflect" "refract" "transpose"
		"ddx" "ddx_coarse" "ddx_fine" "ddy" "ddy_coarse" "ddy_fine" "fwidth"
		"EvaluateAttributeAtCentroid" "EvaluateAttributeAtSample" "EvaluateAttributeSnapped"
		"GetRenderTargetSampleCount" "GetRenderTargetSamplePosition"
		"InterlockedAdd" "InterlockedAnd" "InterlockedCompareExchange" "InterlockedCompareStore" "InterlockedExchange" "InterlockedMax" "InterlockedMin" "InterlockedOr" "InterlockedXor"
		"InterlockedCompareStoreFloatBitwise" "InterlockedCompareExchangeFloatBitwise"
		"Process2DQuadTessFactorsAvg" "Process2DQuadTessFactorsMax" "Process2DQuadTessFactorsMin" "ProcessIsolineTessFactors"
		"ProcessQuadTessFactorsAvg" "ProcessQuadTessFactorsMax" "ProcessQuadTessFactorsMin" "ProcessTriTessFactorsAvg" "ProcessTriTessFactorsMax" "ProcessTriTessFactorsMin"
		"tex1D" "tex1Dbias" "tex1Dgrad" "tex1Dlod" "tex1Dproj"
		"tex2D" "tex2Dbias" "tex2Dgrad" "tex2Dlod" "tex2Dproj"
		"tex3D" "tex3Dbias" "tex3Dgrad" "tex3Dlod" "tex3Dproj"
		"texCUBE" "texCUBEbias" "texCUBEgrad" "texCUBElod" "texCUBEproj"
		"WaveIsFirstLane" "WaveGetLaneCount" "WaveGetLaneIndex"
		"IsHelperLane"
		"WaveActiveAnyTrue" "WaveActiveAllTrue" "WaveActiveBallot"
		"WaveReadLaneFirst" "WaveReadLaneAt"
		"WaveActiveAllEqual" "WaveActiveAllEqualBool" "WaveActiveCountBits"
		"WaveActiveSum" "WaveActiveProduct" "WaveActiveBitAnd" "WaveActiveBitOr" "WaveActiveBitXor" "WaveActiveMin" "WaveActiveMax"
		"WavePrefixCountBits" "WavePrefixProduct" "WavePrefixSum"
		"QuadReadAcrossX" "QuadReadAcrossY" "QuadReadAcrossDiagonal" "QuadReadLaneAt"
		"QuadAny" "QuadAll"
		"WaveMatch" "WaveMultiPrefixSum" "WaveMultiPrefixProduct" "WaveMultiPrefixCountBits" "WaveMultiPrefixAnd" "WaveMultiPrefixOr" "WaveMultiPrefixXor"
		"NonUniformResourceIndex"
		"DispatchMesh" "SetMeshOutputCounts"
		"dot4add_u8packed" "dot4add_i8packed" "dot2add"
		"RestartStrip"
		"CalculateLevelOfDetail" "CalculateLevelOfDetailUnclamped" "Gather" "GetDimensions" "GetSamplePosition" "Load" "Sample" "SampleBias" "SampleCmp" "SampleCmpLevelZero" "SampleGrad" "SampleLevel" "GatherRaw" "SampleCmpLevel"
		"SampleCmpBias" "SampleCmpGrad"
		"WriteSamplerFeedback" "WriteSamplerFeedbackBias" "WriteSamplerFeedbackGrad" "WriteSamplerFeedbackLevel"
		"Append" "Consume" "DecrementCounter" "IncrementCounter"
		"Load2" "Load3" "Load4" "Store" "Store2" "Store3" "Store4"
		"GatherRed" "GatherGreen" "GatherBlue" "GatherAlpha" "GatherCmp" "GatherCmpRed" "GatherCmpGreen" "GatherCmpBlue" "GatherCmpAlpha"
	))

(interface_requirements
  (identifier) @type)

(binary_expression
  [
    "is"
    "as"
  ]
  right: (identifier) @type)

[
  "as"
  "is"
] @keyword.operator

[
  "__exported"
  "import"
] @keyword.control.import

(property_declaration
  (identifier) @variable.other.member)