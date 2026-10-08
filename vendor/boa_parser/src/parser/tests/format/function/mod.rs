use crate::parser::tests::format::test_formatting;

mod class;

#[test]
fn function() {
    test_formatting(
        r#"
        function func(a, b) {
            console.log(a);
        }
        function func_2(a, b) {}
        pass_func(function(a, b) {
            console.log("in callback", a);
        });
        pass_func(function(a, b) {});
        "#,
    );
}

#[test]
fn arrow() {
    test_formatting(
        r#"
        let arrow_func = (a, b) => {
            console.log("in multi statement arrow");
            console.log(b);
        };
        let arrow_func_2 = (a, b) => {};
        "#,
    );
}

// CatPaw: an arrow function's parameters are first parsed as a
// parenthesized expression; their default values and destructuring
// patterns are moved from there into the parameter list, whole.
#[test]
fn arrow_parameters_with_defaults_and_patterns() {
    test_formatting(
        r#"
        let f = (a = 1, { b, c : [ d ] = [2] } = {
            b: 3,
        }, [ e = 4, ... g ] = h, ...i) => {
            return a;
        };
        let k = ({ a, b } = defaults(), [ c ] = []) => {
            return c;
        };
        "#,
    );
}

#[test]
fn r#async() {
    test_formatting(
        r#"
            async function async_func(a, b) {
                console.log(a);
            }
            async function async_func_2(a, b) {}
            pass_async_func(async function(a, b) {
                console.log("in async callback", a);
            });
            pass_async_func(async function(a, b) {});
            "#,
    );
}
