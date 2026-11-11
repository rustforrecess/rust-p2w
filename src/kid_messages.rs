//! The kid reading level for runtime errors.
//!
//! `messages` keeps CPython's wording, which every backend prints and a
//! student should learn to read. This layer sits beside it: it recognises a
//! printed message by its template and says the same thing in plain words,
//! keeping `{slot}` values so the sentence is about the student's own program.

use crate::messages::{self as msg, Msg};

struct Kid {
    msg: &'static Msg,
    says: &'static str,
    next: &'static str,
}

const fn kid(msg: &'static Msg, says: &'static str, next: &'static str) -> Kid {
    Kid { msg, says, next }
}

const TABLE: &[Kid] = &[
    kid(
        &msg::NAME_USE_BEFORE_ASSIGN,
        "A variable was used before it got a value.",
        "Set the variable on a line that runs before this one.",
    ),
    kid(
        &msg::TYPE_EXPECTED_NUMBER,
        "This needs a number, but it got {type}.",
        "If it's text like \"5\", turn it into a number with int(...).",
    ),
    kid(
        &msg::TYPE_NO_LEN,
        "len() counts the items in something, but this is {type}.",
        "Use len() on a list, some text, a dict or a set.",
    ),
    kid(
        &msg::TYPE_NOT_SUBSCRIPTABLE,
        "The [ ] picks one item out of a group, but this is {type}.",
        "Check this variable holds a list, text or a dict.",
    ),
    kid(
        &msg::TYPE_NO_ITEM_ASSIGN,
        "You can't change one item inside {type}.",
        "Build a new value instead, or use a list if you need to change items.",
    ),
    kid(
        &msg::TYPE_ARG_NOT_ITERABLE,
        "This needs a group of items, but this is {type}.",
        "Use a list, some text, a dict or a set here.",
    ),
    kid(
        &msg::TYPE_IN_STRING_LEFT,
        "To look inside text, `in` needs text on its left, but it got {type}.",
        "Turn the left side into text with str(...).",
    ),
    kid(
        &msg::TYPE_STR_UNSUPPORTED,
        "str() can't turn {type} into text yet.",
        "Print the parts inside it one at a time.",
    ),
    kid(
        &msg::TYPE_UNHASHABLE,
        "This is {type}, which can change, so it can't go inside a set or be a dict key.",
        "Use a tuple instead, because a tuple can't change.",
    ),
    kid(
        &msg::TYPE_FOR_NOT_ITERABLE,
        "This is {type}, just one value, so the loop has nothing to go through.",
        "Loop over a list, some text, a dict or a set, or use range(n) to count.",
    ),
    kid(
        &msg::TYPE_METHOD_AS_VALUE,
        "`{name}` is something a thing can do, so it needs to be called.",
        "Add parentheses: `{name}(...)`.",
    ),
    kid(
        &msg::TYPE_SET_OP,
        "This set operation needs a set on both sides.",
        "Turn the other side into a set with set(...).",
    ),
    kid(
        &msg::TYPE_METHOD_ARITY,
        "This method got the wrong number of values.",
        "Check how many values go inside the parentheses.",
    ),
    kid(
        &msg::TYPE_POW_FRACTIONAL,
        "** can use a whole number, or 0.5 for a square root, but not other fractions.",
        "Try math.sqrt(...) or a whole-number power.",
    ),
    kid(
        &msg::ATTR_MISSING,
        "This is {type}, and it doesn't have a part called `{name}`.",
        "Check the spelling after the dot.",
    ),
    kid(
        &msg::ATTR_NO_APPEND,
        "Only a list can append, and this is {type}.",
        "Check this variable holds a list.",
    ),
    kid(
        &msg::ATTR_NOT_FOUND,
        "This object doesn't have a part with that name, or wasn't given it yet.",
        "Check the spelling after the dot, and make sure `__init__` sets it every time, not only inside an `if`.",
    ),
    kid(
        &msg::ATTR_NOT_OBJECT,
        "This is {type}, and only objects made from a class have parts after a dot.",
        "Check what this variable holds just before this line.",
    ),
    kid(
        &msg::ATTR_NO_SLOT,
        "This object doesn't have a part with that name to change.",
        "Check the spelling after the dot, or give the class that part in `__init__`.",
    ),
    kid(
        &msg::ATTR_SLOT_RANGE,
        "Something went wrong inside the compiler, not in your program.",
        "Tell your teacher, and show them this program.",
    ),
    kid(
        &msg::INDEX_OUT_OF_RANGE,
        "You asked for an item past the end of the {seq}.",
        "Items start at 0, so check the number inside the [ ].",
    ),
    kid(
        &msg::INDEX_CHOICE_EMPTY,
        "There's nothing to choose from, because the list is empty.",
        "Add something to the list before choosing.",
    ),
    kid(
        &msg::KEY_MISSING,
        "This dict has no key {key}.",
        "Check the spelling, or add the key first.",
    ),
    kid(
        &msg::VALUE_INT_PARSE,
        "`{text}` isn't a whole number, so int() can't read it.",
        "Check what's in the text, or use float(...) for a decimal.",
    ),
    kid(
        &msg::VALUE_FLOAT_PARSE,
        "`{text}` isn't a number, so float() can't read it.",
        "Check what's in the text.",
    ),
    kid(
        &msg::VALUE_NOT_IN_SEQUENCE,
        "That value isn't in there.",
        "Check with `in` first, like `if x in items:`.",
    ),
    kid(
        &msg::VALUE_EMPTY_SEQUENCE,
        "This needs at least one item, but it's empty.",
        "Check something is in it first.",
    ),
    kid(
        &msg::VALUE_UNPACK_COUNT,
        "The number of names doesn't match the number of values.",
        "Count both sides of the `=`.",
    ),
    kid(
        &msg::VALUE_SLICE_STEP_ZERO,
        "A slice can't move by 0 steps.",
        "Use a step like 1 or 2.",
    ),
    kid(
        &msg::VALUE_RANDINT_ORDER,
        "randint needs the smaller number first.",
        "Swap the two numbers.",
    ),
    kid(
        &msg::ZERO_DIVISION,
        "You can't divide by zero.",
        "Check the number after the division sign before dividing.",
    ),
    kid(
        &msg::FILE_NOT_FOUND,
        "There's no file called `{name}` here.",
        "Check the spelling of the file name.",
    ),
    kid(
        &msg::INT_OVERFLOW,
        "This number got too big to store.",
        "Check for a loop that keeps growing it.",
    ),
];

/// Match `line` against a `{slot}` template, returning each slot's value.
fn match_template<'a>(template: &'a str, line: &'a str) -> Option<Vec<(&'a str, &'a str)>> {
    let mut slots = Vec::new();
    let mut rest = line;
    let mut parts = template.split('{');
    rest = rest.strip_prefix(parts.next()?)?;
    for part in parts {
        let (slot, literal) = part.split_once('}')?;
        let end = if literal.is_empty() {
            rest.len()
        } else {
            rest.find(literal)?
        };
        slots.push((slot, &rest[..end]));
        rest = &rest[end + literal.len()..];
    }
    rest.is_empty().then_some(slots)
}

/// A Python type name in kid words, with its article.
fn kid_type(name: &str) -> String {
    match name {
        "int" => "a whole number".to_string(),
        "float" => "a decimal number".to_string(),
        "str" => "a piece of text".to_string(),
        "bool" => "True or False".to_string(),
        "NoneType" => "None, which means no value".to_string(),
        "unassigned" => "a variable with no value yet".to_string(),
        "list" | "tuple" | "set" | "dict" => format!("a {name}"),
        class if class.starts_with(['a', 'e', 'i', 'o', 'u', 'A', 'E', 'I', 'O', 'U']) => {
            format!("an {class}")
        }
        class => format!("a {class}"),
    }
}

fn fill(text: &str, slots: &[(&str, &str)]) -> String {
    slots.iter().fold(text.to_string(), |out, (slot, value)| {
        let value = match *slot {
            "type" => kid_type(value),
            _ => value.to_string(),
        };
        out.replace(&format!("{{{slot}}}"), &value)
    })
}

/// Say `line` in plain words, if it's a runtime error. A more specific
/// template wins, so `attr.no-append` beats `attr.missing`.
fn explain_line(line: &str) -> Option<String> {
    let literal_len = |k: &&Kid| k.msg.text.len();
    let k = TABLE
        .iter()
        .filter(|k| match_template(k.msg.text, line).is_some())
        .max_by_key(literal_len)?;
    let slots = match_template(k.msg.text, line)?;
    Some(format!(
        "{}\nTry: {}",
        fill(k.says, &slots),
        fill(k.next, &slots)
    ))
}

/// Find the runtime error in a program's output and say it in plain words.
/// It's the last thing printed, because the program stops there.
pub fn explain_runtime_error(output: &str) -> Option<String> {
    explain_line(output.trim_end().lines().last()?.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runtime_message_has_a_kid_version() {
        let declared = include_str!("messages.rs").matches("Msg = m(").count();
        assert_eq!(TABLE.len(), declared);
        let keys: std::collections::HashSet<_> = TABLE.iter().map(|k| k.msg.key).collect();
        assert_eq!(keys.len(), TABLE.len(), "a message is listed twice");
    }

    #[test]
    fn kid_text_only_uses_slots_its_message_has() {
        for k in TABLE {
            for text in [k.says, k.next] {
                for part in text.split('{').skip(1) {
                    let slot = part.split_once('}').expect("closed slot").0;
                    let wanted = format!("{{{slot}}}");
                    assert!(k.msg.text.contains(&wanted), "{}: {slot}", k.msg.key);
                }
            }
        }
    }

    #[test]
    fn explains_with_the_program_s_own_values() {
        for (output, want) in [
            (
                "hello\nIndexError: list index out of range\n",
                "You asked for an item past the end of the list.\n\
                 Try: Items start at 0, so check the number inside the [ ].",
            ),
            (
                "KeyError: 'speed'\n",
                "This dict has no key 'speed'.\nTry: Check the spelling, or add the key first.",
            ),
            (
                "ValueError: invalid literal for int() with base 10: 'abc'\n",
                "`abc` isn't a whole number, so int() can't read it.\n\
                 Try: Check what's in the text, or use float(...) for a decimal.",
            ),
            (
                "ZeroDivisionError: division by zero\n",
                "You can't divide by zero.\n\
                 Try: Check the number after the division sign before dividing.",
            ),
        ] {
            assert_eq!(explain_runtime_error(output).as_deref(), Some(want));
        }
    }

    #[test]
    fn type_names_become_kid_words() {
        let got = explain_runtime_error("TypeError: 'int' object is not subscriptable").unwrap();
        assert!(got.contains("but this is a whole number."), "{got}");
        let got = explain_runtime_error("TypeError: expected a number, got 'str'").unwrap();
        assert!(got.contains("it got a piece of text."), "{got}");
        let got = explain_runtime_error("AttributeError: 'Robot' object has no attribute 'wings'")
            .unwrap();
        assert!(got.contains("This is a Robot, and it doesn't have a part called `wings`."));
    }

    #[test]
    fn the_more_specific_message_wins() {
        let got = explain_runtime_error("AttributeError: 'int' object has no attribute 'append'");
        assert!(got.unwrap().starts_with("Only a list can append"));
    }

    #[test]
    fn ordinary_output_is_left_alone() {
        assert_eq!(explain_runtime_error("hello\nworld\n"), None);
        assert_eq!(explain_runtime_error(""), None);
    }
}
