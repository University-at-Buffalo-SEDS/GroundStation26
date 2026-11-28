//! A small sandboxed expression language for operator-defined DAC
//! waveform shapes: f(x) over one period (x in [0, 1)) returning -1..1, e.g.
//! `0.8*sin(2*pi*x) + 0.2*sign(sin(2*pi*10*x))`. `t = x / f` (seconds) is
//! there for convenience: `0.5 + 0.4*sin(2*pi*50*t) + 0.1*sign(sin(2*pi*5*t))`.
//!
//! It parses arithmetic over numbers, a fixed set of variables and constants,
//! and a whitelist of pure math functions into a tree, then evaluates that tree
//! in f64. Nothing else exists: no assignment, no strings, no loops, no
//! lookups beyond the whitelist, and nothing reaches the board (the GS
//! samples the expression into DAC codes). Input size, tree size and nesting
//! depth are bounded.
use serde::Serialize;

pub const MAX_LEN: usize = 512;
const MAX_NODES: usize = 512;
const MAX_DEPTH: usize = 256;

/// Values available while sampling one table entry.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vars {
    /// Seconds since the start of the table.
    pub t: f64,
    /// Sample index.
    pub n: f64,
    /// Table length.
    pub len: f64,
    /// Fraction of the table, n / N (0..1).
    pub x: f64,
    /// Table period, seconds.
    pub period: f64,
    /// Table repetition frequency, 1 / T.
    pub freq: f64,
    /// Sample rate, N / T.
    pub rate: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Var {
    T,
    N,
    Len,
    X,
    Period,
    Freq,
    Rate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Func {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sinh,
    Cosh,
    Tanh,
    Abs,
    Sign,
    Sqrt,
    Exp,
    Ln,
    Log10,
    Log2,
    Floor,
    Ceil,
    Round,
    Trunc,
    Frac,
    Mod,
    Pow,
    Min,
    Max,
    Clamp,
    Step,
}

/// (name, function, minimum args, maximum args)
const FUNCTIONS: &[(&str, Func, usize, usize)] = &[
    ("sin", Func::Sin, 1, 1),
    ("cos", Func::Cos, 1, 1),
    ("tan", Func::Tan, 1, 1),
    ("asin", Func::Asin, 1, 1),
    ("acos", Func::Acos, 1, 1),
    ("atan", Func::Atan, 1, 1),
    ("atan2", Func::Atan2, 2, 2),
    ("sinh", Func::Sinh, 1, 1),
    ("cosh", Func::Cosh, 1, 1),
    ("tanh", Func::Tanh, 1, 1),
    ("abs", Func::Abs, 1, 1),
    ("sign", Func::Sign, 1, 1),
    ("sqrt", Func::Sqrt, 1, 1),
    ("exp", Func::Exp, 1, 1),
    ("log", Func::Ln, 1, 1),
    ("ln", Func::Ln, 1, 1),
    ("log10", Func::Log10, 1, 1),
    ("log2", Func::Log2, 1, 1),
    ("floor", Func::Floor, 1, 1),
    ("ceil", Func::Ceil, 1, 1),
    ("round", Func::Round, 1, 1),
    ("trunc", Func::Trunc, 1, 1),
    ("frac", Func::Frac, 1, 1),
    ("mod", Func::Mod, 2, 2),
    ("pow", Func::Pow, 2, 2),
    ("min", Func::Min, 2, 16),
    ("max", Func::Max, 2, 16),
    ("clamp", Func::Clamp, 3, 3),
    ("step", Func::Step, 1, 1),
];

const VARIABLES: &[(&str, Var)] = &[
    ("t", Var::T),
    ("n", Var::N),
    ("N", Var::Len),
    ("x", Var::X),
    ("T", Var::Period),
    ("f", Var::Freq),
    ("fs", Var::Rate),
];

const CONSTANTS: &[(&str, f64)] = &[
    ("pi", std::f64::consts::PI),
    ("tau", std::f64::consts::TAU),
    ("e", std::f64::consts::E),
];

/// The vocabulary, for the UI's help text.
pub fn vocabulary() -> serde_json::Value {
    serde_json::json!({
        "variables": {
            "t": "seconds since the period start, x/f",
            "n": "sample index",
            "N": "table length (samples)",
            "x": "fraction of the period, n/N (0..1): the shape's main variable",
            "T": "period (s)",
            "f": "repetition frequency (Hz)",
            "fs": "sample rate (S/s)",
        },
        "constants": CONSTANTS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        "functions": FUNCTIONS.iter().map(|(n, _, lo, hi)| {
            if lo == hi { format!("{n}/{lo}") } else { format!("{n}/{lo}..{hi}") }
        }).collect::<Vec<_>>(),
        "operators": ["+", "-", "*", "/", "%", "^", "( )"],
        "max_length": MAX_LEN,
    })
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Num(f64),
    Var(Var),
    Neg(Box<Node>),
    Bin(char, Box<Node>, Box<Node>),
    Call(Func, Vec<Node>),
}

/// A parse or evaluation error, with the character position when known.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExprError {
    pub message: String,
    pub position: Option<usize>,
}
impl std::fmt::Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.position {
            Some(p) => write!(f, "{} (at character {})", self.message, p + 1),
            None => write!(f, "{}", self.message),
        }
    }
}
fn err<T>(message: impl Into<String>, position: usize) -> Result<T, ExprError> {
    Err(ExprError {
        message: message.into(),
        position: Some(position),
    })
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Comma,
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            let value: f64 = text
                .parse()
                .map_err(|_| ExprError {
                    message: format!("{text:?} is not a number"),
                    position: Some(start),
                })?;
            if !value.is_finite() {
                return err(format!("{text} is too large"), start);
            }
            out.push((Tok::Num(value), start));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push((Tok::Ident(chars[start..i].iter().collect()), start));
            continue;
        }
        let tok = match c {
            '+' | '-' | '*' | '/' | '%' | '^' => Tok::Op(c),
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            _ => return err(format!("Unexpected character {c:?}"), start),
        };
        out.push((tok, start));
        i += 1;
    }
    Ok(out)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    nodes: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }
    fn at(&self) -> usize {
        self.toks.get(self.pos).map(|(_, p)| *p).unwrap_or(self.end)
    }
    fn node(&mut self, node: Node) -> Result<Node, ExprError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return err(
                format!("The expression is too complex (over {MAX_NODES} terms)"),
                self.at(),
            );
        }
        Ok(node)
    }
    fn depth(&self, depth: usize) -> Result<(), ExprError> {
        if depth > MAX_DEPTH {
            err(
                format!("The expression nests too deeply (over {MAX_DEPTH} levels)"),
                self.at(),
            )
        } else {
            Ok(())
        }
    }
    fn expr(&mut self, depth: usize) -> Result<Node, ExprError> {
        self.depth(depth)?;
        let mut left = self.term(depth + 1)?;
        while let Some(Tok::Op(op @ ('+' | '-'))) = self.peek().cloned() {
            self.pos += 1;
            let right = self.term(depth + 1)?;
            left = self.node(Node::Bin(op, Box::new(left), Box::new(right)))?;
        }
        Ok(left)
    }
    fn term(&mut self, depth: usize) -> Result<Node, ExprError> {
        self.depth(depth)?;
        let mut left = self.unary(depth + 1)?;
        while let Some(Tok::Op(op @ ('*' | '/' | '%'))) = self.peek().cloned() {
            self.pos += 1;
            let right = self.unary(depth + 1)?;
            left = self.node(Node::Bin(op, Box::new(left), Box::new(right)))?;
        }
        Ok(left)
    }
    fn unary(&mut self, depth: usize) -> Result<Node, ExprError> {
        self.depth(depth)?;
        match self.peek() {
            Some(Tok::Op('-')) => {
                self.pos += 1;
                let inner = self.unary(depth + 1)?;
                self.node(Node::Neg(Box::new(inner)))
            }
            Some(Tok::Op('+')) => {
                self.pos += 1;
                self.unary(depth + 1)
            }
            _ => self.power(depth + 1),
        }
    }
    fn power(&mut self, depth: usize) -> Result<Node, ExprError> {
        self.depth(depth)?;
        let base = self.atom(depth + 1)?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            // Right associative; binds tighter than unary minus on its left.
            let exponent = self.unary(depth + 1)?;
            return self.node(Node::Bin('^', Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }
    fn atom(&mut self, depth: usize) -> Result<Node, ExprError> {
        self.depth(depth)?;
        let at = self.at();
        let Some((tok, _)) = self.toks.get(self.pos).cloned() else {
            return err("The expression ends early", at);
        };
        self.pos += 1;
        match tok {
            Tok::Num(v) => self.node(Node::Num(v)),
            Tok::LParen => {
                let inner = self.expr(depth + 1)?;
                if self.peek() != Some(&Tok::RParen) {
                    return err("Missing ')'", self.at());
                }
                self.pos += 1;
                Ok(inner)
            }
            Tok::Ident(name) => {
                if self.peek() == Some(&Tok::LParen) {
                    let Some(&(_, func, lo, hi)) = FUNCTIONS.iter().find(|(n, ..)| *n == name)
                    else {
                        return err(format!("Unknown function {name:?}"), at);
                    };
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.expr(depth + 1)?);
                            match self.peek() {
                                Some(Tok::Comma) => self.pos += 1,
                                Some(Tok::RParen) => break,
                                _ => return err("Expected ',' or ')'", self.at()),
                            }
                        }
                    }
                    self.pos += 1;
                    if args.len() < lo || args.len() > hi {
                        let want = if lo == hi {
                            format!("{lo}")
                        } else {
                            format!("{lo} to {hi}")
                        };
                        return err(
                            format!("{name}() takes {want} argument(s), not {}", args.len()),
                            at,
                        );
                    }
                    return self.node(Node::Call(func, args));
                }
                if let Some(&(_, v)) = VARIABLES.iter().find(|(n, _)| *n == name) {
                    return self.node(Node::Var(v));
                }
                if let Some(&(_, v)) = CONSTANTS.iter().find(|(n, _)| *n == name) {
                    return self.node(Node::Num(v));
                }
                if FUNCTIONS.iter().any(|(n, ..)| *n == name) {
                    return err(format!("{name} is a function; write {name}(...)"), at);
                }
                err(format!("Unknown name {name:?}"), at)
            }
            Tok::Op(op) => err(format!("Unexpected '{op}'"), at),
            Tok::RParen => err("Unexpected ')'", at),
            Tok::Comma => err("Unexpected ','", at),
        }
    }
}

/// A parsed, validated expression.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    root: Node,
    pub source: String,
}

impl Expr {
    pub fn parse(source: &str) -> Result<Self, ExprError> {
        if source.trim().is_empty() {
            return Err(ExprError {
                message: "Enter an expression".into(),
                position: None,
            });
        }
        if source.chars().count() > MAX_LEN {
            return Err(ExprError {
                message: format!("The expression is longer than {MAX_LEN} characters"),
                position: Some(MAX_LEN),
            });
        }
        let toks = lex(source)?;
        let mut parser = Parser {
            toks,
            pos: 0,
            nodes: 0,
            end: source.chars().count(),
        };
        let root = parser.expr(0)?;
        if parser.pos != parser.toks.len() {
            return err("Unexpected text after the expression", parser.at());
        }
        Ok(Self {
            root,
            source: source.to_string(),
        })
    }

    pub fn eval(&self, vars: &Vars) -> f64 {
        eval(&self.root, vars)
    }

    /// Does the shape depend on the playback frequency (t, T, f or fs)? Such a
    /// shape is re-sampled when the frequency changes.
    pub fn uses_time(&self) -> bool {
        fn walk(node: &Node) -> bool {
            match node {
                Node::Num(_) => false,
                Node::Var(v) => matches!(v, Var::T | Var::Period | Var::Freq | Var::Rate),
                Node::Neg(a) => walk(a),
                Node::Bin(_, a, b) => walk(a) || walk(b),
                Node::Call(_, args) => args.iter().any(walk),
            }
        }
        walk(&self.root)
    }

    /// Does the value depend on the sample position at all?
    pub fn is_constant(&self) -> bool {
        fn uses_var(node: &Node) -> bool {
            match node {
                Node::Num(_) => false,
                Node::Var(_) => true,
                Node::Neg(a) => uses_var(a),
                Node::Bin(_, a, b) => uses_var(a) || uses_var(b),
                Node::Call(_, args) => args.iter().any(uses_var),
            }
        }
        !uses_var(&self.root)
    }
}

fn floor_mod(a: f64, b: f64) -> f64 {
    a - b * (a / b).floor()
}

fn eval(node: &Node, v: &Vars) -> f64 {
    match node {
        Node::Num(x) => *x,
        Node::Var(var) => match var {
            Var::T => v.t,
            Var::N => v.n,
            Var::Len => v.len,
            Var::X => v.x,
            Var::Period => v.period,
            Var::Freq => v.freq,
            Var::Rate => v.rate,
        },
        Node::Neg(a) => -eval(a, v),
        Node::Bin(op, a, b) => {
            let (a, b) = (eval(a, v), eval(b, v));
            match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                '%' => floor_mod(a, b),
                _ => a.powf(b),
            }
        }
        Node::Call(func, args) => {
            let a: Vec<f64> = args.iter().map(|n| eval(n, v)).collect();
            let x = a[0];
            match func {
                Func::Sin => x.sin(),
                Func::Cos => x.cos(),
                Func::Tan => x.tan(),
                Func::Asin => x.asin(),
                Func::Acos => x.acos(),
                Func::Atan => x.atan(),
                Func::Atan2 => x.atan2(a[1]),
                Func::Sinh => x.sinh(),
                Func::Cosh => x.cosh(),
                Func::Tanh => x.tanh(),
                Func::Abs => x.abs(),
                Func::Sign => {
                    if x > 0.0 {
                        1.0
                    } else if x < 0.0 {
                        -1.0
                    } else if x == 0.0 {
                        0.0
                    } else {
                        f64::NAN
                    }
                }
                Func::Sqrt => x.sqrt(),
                Func::Exp => x.exp(),
                Func::Ln => x.ln(),
                Func::Log10 => x.log10(),
                Func::Log2 => x.log2(),
                Func::Floor => x.floor(),
                Func::Ceil => x.ceil(),
                Func::Round => x.round(),
                Func::Trunc => x.trunc(),
                Func::Frac => x - x.floor(),
                Func::Mod => floor_mod(x, a[1]),
                Func::Pow => x.powf(a[1]),
                Func::Min => a.iter().copied().fold(f64::INFINITY, f64::min),
                Func::Max => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                Func::Clamp => {
                    if a[1] > a[2] || a[1].is_nan() || a[2].is_nan() {
                        f64::NAN
                    } else {
                        x.clamp(a[1], a[2])
                    }
                }
                Func::Step => {
                    if x >= 0.0 {
                        1.0
                    } else if x < 0.0 {
                        0.0
                    } else {
                        f64::NAN
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(src: &str) -> f64 {
        Expr::parse(src).unwrap().eval(&Vars::default())
    }
    fn at(src: &str, vars: Vars) -> f64 {
        Expr::parse(src).unwrap().eval(&vars)
    }
    fn error(src: &str) -> ExprError {
        Expr::parse(src).unwrap_err()
    }

    #[test]
    fn arithmetic_precedence_and_associativity() {
        assert_eq!(val("1 + 2 * 3"), 7.0);
        assert_eq!(val("(1 + 2) * 3"), 9.0);
        assert_eq!(val("2 ^ 3 ^ 2"), 512.0);
        assert_eq!(val("-2 ^ 2"), -4.0);
        assert_eq!(val("2 ^ -1"), 0.5);
        assert_eq!(val("10 - 4 - 3"), 3.0);
        assert_eq!(val("12 / 3 / 2"), 2.0);
        assert_eq!(val("-7 % 3"), 2.0);
        assert_eq!(val("mod(7.5, 2)"), 1.5);
        assert_eq!(val("--3"), 3.0);
        assert_eq!(val("+3"), 3.0);
        assert_eq!(val("1.5e3 + .5 + 2E-1"), 1500.7);
    }

    #[test]
    fn functions_constants_and_variables() {
        assert!((val("sin(pi/2)") - 1.0).abs() < 1e-15);
        assert!((val("cos(tau)") - 1.0).abs() < 1e-15);
        assert!((val("log(e)") - 1.0).abs() < 1e-15);
        assert_eq!(val("log10(1000)"), 3.0);
        assert_eq!(val("sqrt(16) + abs(-2) + floor(2.7) + ceil(2.1) + round(2.5)"), 4.0 + 2.0 + 2.0 + 3.0 + 3.0);
        assert_eq!(val("min(3, 1, 2) + max(3, 1, 2)"), 4.0);
        assert_eq!(val("clamp(5, 0, 1) + clamp(-5, 0, 1)"), 1.0);
        assert_eq!(val("sign(-3) + sign(0) + sign(2)"), 0.0);
        assert_eq!(val("step(-1) + step(0)"), 1.0);
        assert_eq!(val("frac(2.25)"), 0.25);
        assert_eq!(val("atan2(1, 1) * 4"), std::f64::consts::PI);
        assert_eq!(val("pow(2, 10)"), 1024.0);
        let v = Vars {
            t: 0.25,
            n: 3.0,
            len: 8.0,
            x: 0.375,
            period: 2.0,
            freq: 0.5,
            rate: 4.0,
        };
        assert_eq!(at("t + n + N + x + T + f + fs", v), 0.25 + 3.0 + 8.0 + 0.375 + 2.0 + 0.5 + 4.0);
        // The operator's example.
        let e = Expr::parse("0.5 + 0.4*sin(2*pi*50*t) + 0.1*sign(sin(2*pi*5*t))").unwrap();
        let y = e.eval(&Vars { t: 0.005, ..Vars::default() });
        assert!((y - (0.5 + 0.4 + 0.1)).abs() < 1e-12);
        let y = e.eval(&Vars { t: 0.115, ..Vars::default() });
        assert!((y - (0.5 + 0.4 * (2.0 * std::f64::consts::PI * 50.0 * 0.115).sin() - 0.1)).abs() < 1e-12);
        assert!(!e.is_constant() && e.uses_time());
        assert!(Expr::parse("2*pi").unwrap().is_constant());
        assert!(!Expr::parse("sin(2*pi*x) + n/N").unwrap().uses_time());
        assert!(Expr::parse("x*fs").unwrap().uses_time());
    }

    #[test]
    fn unsafe_and_malformed_input_is_rejected() {
        for src in [
            "import os",
            "__import__('os')",
            "eval(1)",
            "exec(1)",
            "system(1)",
            "t; 1",
            "t = 1",
            "a = 1",
            "\"text\"",
            "'x'",
            "x[0]",
            "x.y",
            "{1}",
            "1 && 2",
            "1 == 1",
            "$t",
            "`ls`",
            "sin",
            "sin()",
            "sin(1, 2)",
            "clamp(1, 2)",
            "min(1)",
            "foo(1)",
            "y",
            "1 +",
            "(1",
            "1)",
            "1 2",
            "",
            "   ",
            ",",
            "1e999",
            "2..3",
        ] {
            assert!(Expr::parse(src).is_err(), "{src:?} must be rejected");
        }
        assert_eq!(error("foo(1)").message, "Unknown function \"foo\"");
        assert_eq!(error("1 + bar").position, Some(4));
        assert_eq!(error("sin").message, "sin is a function; write sin(...)");
        assert!(error("sin(1, 2)").message.contains("takes 1 argument"));
        // Size and depth bounds.
        assert!(Expr::parse(&"1+".repeat(300)).is_err());
        assert!(Expr::parse(&format!("{}1{}", "(".repeat(60), ")".repeat(60))).is_err());
        assert!(Expr::parse(&format!("{}1{}", "(".repeat(10), ")".repeat(10))).is_ok());
        assert!(Expr::parse(&"-".repeat(100)).is_err());
        let long = format!("1{}", "+1".repeat(255));
        assert!(long.len() <= MAX_LEN && Expr::parse(&long).is_ok());
    }

    #[test]
    fn non_finite_results_surface_as_values_not_panics() {
        assert!(val("1/0").is_infinite());
        assert!(val("log(0)").is_infinite());
        assert!(val("sqrt(-1)").is_nan());
        assert!(val("clamp(1, 2, 0)").is_nan());
        assert!(val("mod(1, 0)").is_nan());
    }
}
