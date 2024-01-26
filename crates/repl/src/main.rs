use interpreter::Runner;
use lexer::Lexer;
use parser::{ast, Parse, ParseStream};

//

fn main() {
    let str = r#"
        const := 4;

        main := fn() -> i32 {
            print("Hello world ", const);
            return 0;
        };
    "#;
    println!("{str}");

    let mut stream = ParseStream::from_lexer(Lexer::new(str));
    let ast: Result<ast::Ast<ast::Root>, _> =
        Parse::parse(&mut stream).map_err(|err| err.to_string());
    let extra = stream
        .next()
        .transpose()
        .map(|t| t.map(|t| t.as_str().to_string()))
        .map_err(|err| err.to_string());

    println!("{ast:?} {extra:?}");

    if let Ok(ast) = ast {
        let mut runner = Runner::new();
        runner.add_defaults();

        let eval = runner.exec(&ast);
        println!("{eval:?}");
    }
}
