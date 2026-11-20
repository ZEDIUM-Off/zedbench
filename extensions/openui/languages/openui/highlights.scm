(comment) @comment

(string) @string
(escape_sequence) @string.escape
(number) @number
[(true) (false)] @boolean
(null) @constant.builtin

; `$name` state that bindings and `@Set` change.
(state) @variable.special

(statement name: (identifier) @variable)
(statement name: (type_identifier) @type)

; Components.
(call function: (type_identifier) @type)
(type_identifier) @type

; `@Each`, `@Set`, `@ToAssistant`…
(action) @function.builtin

(property_identifier) @property
(identifier) @variable

[
  "=" "==" "!=" ">" "<" ">=" "<="
  "+" "-" "*" "/" "%" "!" "&&" "||"
  "?" ":"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "."] @punctuation.delimiter
