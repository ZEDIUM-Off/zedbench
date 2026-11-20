/**
 * @file OpenUI Lang grammar, after the lexer and parser of openui-rs.
 *
 * A program is a list of `name = expression` statements. Newlines are extras:
 * a statement ends where its expression cannot continue, which matches how
 * OpenUI splits statements outside brackets.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  ternary: 1,
  or: 2,
  and: 3,
  equality: 4,
  comparison: 5,
  additive: 6,
  multiplicative: 7,
  unary: 8,
  postfix: 9,
};

const commaSep = (rule) => optional(seq(rule, repeat(seq(",", rule)), optional(",")));

module.exports = grammar({
  name: "openui",

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  rules: {
    source_file: ($) => repeat($.statement),

    statement: ($) =>
      seq(
        field("name", choice($.identifier, $.type_identifier, $.state)),
        "=",
        field("value", $._expression),
      ),

    _expression: ($) =>
      choice(
        $.call,
        $.action_call,
        $.array,
        $.object,
        $.member_expression,
        $.subscript_expression,
        $.unary_expression,
        $.binary_expression,
        $.ternary_expression,
        $.parenthesized_expression,
        $.string,
        $.number,
        $.true,
        $.false,
        $.null,
        $.state,
        $.action,
        $.type_identifier,
        $.identifier,
      ),

    call: ($) =>
      prec(PREC.postfix, seq(field("function", $.type_identifier), field("arguments", $.arguments))),

    action_call: ($) =>
      prec(PREC.postfix, seq(field("function", $.action), field("arguments", $.arguments))),

    arguments: ($) => seq("(", commaSep($._expression), ")"),

    array: ($) => seq("[", commaSep($._expression), "]"),

    object: ($) => seq("{", commaSep($.pair), "}"),

    pair: ($) =>
      seq(
        field("key", choice($._property, $.string, $.number, $.state)),
        ":",
        field("value", $._expression),
      ),

    member_expression: ($) =>
      prec(
        PREC.postfix,
        seq(
          field("object", $._expression),
          ".",
          field("property", choice($._property, $.number)),
        ),
      ),

    subscript_expression: ($) =>
      prec(
        PREC.postfix,
        seq(field("object", $._expression), "[", field("index", $._expression), "]"),
      ),

    unary_expression: ($) =>
      prec(PREC.unary, seq(field("operator", choice("!", "-")), field("argument", $._expression))),

    binary_expression: ($) =>
      choice(
        ...[
          ["||", PREC.or],
          ["&&", PREC.and],
          ["==", PREC.equality],
          ["!=", PREC.equality],
          [">", PREC.comparison],
          ["<", PREC.comparison],
          [">=", PREC.comparison],
          ["<=", PREC.comparison],
          ["+", PREC.additive],
          ["-", PREC.additive],
          ["*", PREC.multiplicative],
          ["/", PREC.multiplicative],
          ["%", PREC.multiplicative],
        ].map(([operator, precedence]) =>
          prec.left(
            precedence,
            seq(
              field("left", $._expression),
              field("operator", operator),
              field("right", $._expression),
            ),
          ),
        ),
      ),

    ternary_expression: ($) =>
      prec.right(
        PREC.ternary,
        seq(
          field("condition", $._expression),
          "?",
          field("consequence", $._expression),
          ":",
          field("alternative", $._expression),
        ),
      ),

    _property: ($) =>
      choice(
        alias($.identifier, $.property_identifier),
        alias($.type_identifier, $.property_identifier),
      ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    string: ($) =>
      choice(
        seq('"', repeat(choice($.string_content, $.escape_sequence)), '"'),
        seq("'", repeat(choice(alias($._single_string_content, $.string_content), $.escape_sequence)), "'"),
      ),

    string_content: (_) => token.immediate(prec(1, /[^"\\]+/)),
    _single_string_content: (_) => token.immediate(prec(1, /[^'\\]+/)),
    escape_sequence: (_) => token.immediate(seq("\\", /(u[0-9a-fA-F]{4}|.|\n)/)),

    number: (_) => token(seq(/[0-9]+/, optional(seq(".", /[0-9]+/)), optional(seq(/[eE]/, optional(/[+-]/), /[0-9]*/)))),

    true: (_) => "true",
    false: (_) => "false",
    null: (_) => "null",

    state: (_) => /\$[a-zA-Z_][a-zA-Z0-9_]*/,
    action: (_) => /@[a-zA-Z_][a-zA-Z0-9_]*/,
    type_identifier: (_) => /[A-Z][a-zA-Z0-9_]*/,
    identifier: (_) => /[a-z_][a-zA-Z0-9_]*/,

    comment: (_) => token(choice(seq("//", /[^\n]*/), seq("#", /[^\n]*/))),
  },
});
