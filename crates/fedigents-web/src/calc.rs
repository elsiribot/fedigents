use anyhow::bail;

pub fn evaluate(expr: &str) -> anyhow::Result<f64> {
    let tokens = tokenize(expr)?;
    let mut pos = 0;
    let result = parse_expr(&tokens, &mut pos)?;
    if pos != tokens.len() {
        bail!("Unexpected token at position {pos}");
    }
    Ok(result)
}

#[derive(Debug, Clone)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Mul,
    Div,
    Pow,
    LParen,
    RParen,
}

fn tokenize(expr: &str) -> anyhow::Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' => {
                chars.next();
            }
            '0'..='9' | '.' => {
                let mut num = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() || c == '.' || c == '_' {
                        if c != '_' {
                            num.push(c);
                        }
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Num(
                    num.parse::<f64>()
                        .map_err(|_| anyhow::anyhow!("Invalid number: {num}"))?,
                ));
            }
            '+' => {
                tokens.push(Token::Plus);
                chars.next();
            }
            '-' => {
                let is_unary = matches!(
                    tokens.last(),
                    None | Some(Token::Plus)
                        | Some(Token::Minus)
                        | Some(Token::Mul)
                        | Some(Token::Div)
                        | Some(Token::Pow)
                        | Some(Token::LParen)
                );
                if is_unary {
                    chars.next();
                    let mut num = String::from("-");
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_digit() || c == '.' || c == '_' {
                            if c != '_' {
                                num.push(c);
                            }
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if num.len() > 1 {
                        tokens.push(Token::Num(
                            num.parse::<f64>()
                                .map_err(|_| anyhow::anyhow!("Invalid number: {num}"))?,
                        ));
                    } else {
                        // minus followed by paren or something else: push 0 - x
                        tokens.push(Token::Num(0.0));
                        tokens.push(Token::Minus);
                    }
                } else {
                    tokens.push(Token::Minus);
                    chars.next();
                }
            }
            '*' => {
                chars.next();
                if chars.peek() == Some(&'*') {
                    chars.next();
                    tokens.push(Token::Pow);
                } else {
                    tokens.push(Token::Mul);
                }
            }
            '/' => {
                tokens.push(Token::Div);
                chars.next();
            }
            '^' => {
                tokens.push(Token::Pow);
                chars.next();
            }
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            _ => bail!("Unexpected character: {c}"),
        }
    }
    Ok(tokens)
}

fn parse_expr(tokens: &[Token], pos: &mut usize) -> anyhow::Result<f64> {
    let mut left = parse_term(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Plus => {
                *pos += 1;
                left += parse_term(tokens, pos)?;
            }
            Token::Minus => {
                *pos += 1;
                left -= parse_term(tokens, pos)?;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn parse_term(tokens: &[Token], pos: &mut usize) -> anyhow::Result<f64> {
    let mut left = parse_power(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Mul => {
                *pos += 1;
                left *= parse_power(tokens, pos)?;
            }
            Token::Div => {
                *pos += 1;
                left /= parse_power(tokens, pos)?;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn parse_power(tokens: &[Token], pos: &mut usize) -> anyhow::Result<f64> {
    let base = parse_atom(tokens, pos)?;
    if *pos < tokens.len() && matches!(tokens[*pos], Token::Pow) {
        *pos += 1;
        let exp = parse_power(tokens, pos)?; // right-associative
        Ok(base.powf(exp))
    } else {
        Ok(base)
    }
}

fn parse_atom(tokens: &[Token], pos: &mut usize) -> anyhow::Result<f64> {
    if *pos >= tokens.len() {
        bail!("Unexpected end of expression");
    }
    match &tokens[*pos] {
        Token::Num(n) => {
            let val = *n;
            *pos += 1;
            Ok(val)
        }
        Token::LParen => {
            *pos += 1;
            let val = parse_expr(tokens, pos)?;
            if *pos >= tokens.len() || !matches!(tokens[*pos], Token::RParen) {
                bail!("Missing closing parenthesis");
            }
            *pos += 1;
            Ok(val)
        }
        _ => bail!("Unexpected token at position {pos}"),
    }
}

#[cfg(test)]
mod tests {
    use super::evaluate;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn literal() {
        assert!(approx(evaluate("42").unwrap(), 42.0));
    }

    #[test]
    fn decimal() {
        assert!(approx(evaluate("3.14").unwrap(), 3.14));
    }

    #[test]
    fn underscore_separator() {
        assert!(approx(evaluate("100_000").unwrap(), 100_000.0));
    }

    #[test]
    fn addition() {
        assert!(approx(evaluate("2 + 3").unwrap(), 5.0));
    }

    #[test]
    fn subtraction() {
        assert!(approx(evaluate("10 - 4").unwrap(), 6.0));
    }

    #[test]
    fn multiplication() {
        assert!(approx(evaluate("6 * 7").unwrap(), 42.0));
    }

    #[test]
    fn division() {
        assert!(approx(evaluate("20 / 4").unwrap(), 5.0));
    }

    #[test]
    fn operator_precedence() {
        // 2 + 3 * 4 = 14, not 20
        assert!(approx(evaluate("2 + 3 * 4").unwrap(), 14.0));
    }

    #[test]
    fn parentheses() {
        assert!(approx(evaluate("(2 + 3) * 4").unwrap(), 20.0));
    }

    #[test]
    fn nested_parentheses() {
        assert!(approx(evaluate("((1 + 2) * (3 + 4))").unwrap(), 21.0));
    }

    #[test]
    fn power_caret() {
        assert!(approx(evaluate("2 ^ 10").unwrap(), 1024.0));
    }

    #[test]
    fn power_double_star() {
        assert!(approx(evaluate("2 ** 10").unwrap(), 1024.0));
    }

    #[test]
    fn power_right_associative() {
        // 2^3^2 = 2^(3^2) = 2^9 = 512, not (2^3)^2 = 64
        assert!(approx(evaluate("2^3^2").unwrap(), 512.0));
    }

    #[test]
    fn power_precedence_over_multiply() {
        // 3 * 2^3 = 3 * 8 = 24
        assert!(approx(evaluate("3 * 2^3").unwrap(), 24.0));
    }

    #[test]
    fn unary_minus_number() {
        assert!(approx(evaluate("-5").unwrap(), -5.0));
    }

    #[test]
    fn unary_minus_in_expression() {
        assert!(approx(evaluate("3 + -2").unwrap(), 1.0));
    }

    #[test]
    fn unary_minus_parenthesized() {
        assert!(approx(evaluate("-(3 + 2)").unwrap(), -5.0));
    }

    #[test]
    fn complex_expression() {
        // 1500 * 0.03 + 20 = 45 + 20 = 65
        assert!(approx(evaluate("1500 * 0.03 + 20").unwrap(), 65.0));
    }

    #[test]
    fn whitespace_variations() {
        assert!(approx(evaluate("  1+2  ").unwrap(), 3.0));
        assert!(approx(evaluate("1\t+\n2").unwrap(), 3.0));
    }

    #[test]
    fn division_produces_float() {
        assert!(approx(evaluate("7 / 2").unwrap(), 3.5));
    }

    #[test]
    fn chained_operations() {
        assert!(approx(evaluate("1 + 2 + 3 + 4").unwrap(), 10.0));
        assert!(approx(evaluate("2 * 3 * 4").unwrap(), 24.0));
    }

    #[test]
    fn mixed_precedence() {
        // 2 + 3 * 4 - 1 = 2 + 12 - 1 = 13
        assert!(approx(evaluate("2 + 3 * 4 - 1").unwrap(), 13.0));
    }

    #[test]
    fn empty_expression_errors() {
        assert!(evaluate("").is_err());
    }

    #[test]
    fn missing_closing_paren_errors() {
        assert!(evaluate("(1 + 2").is_err());
    }

    #[test]
    fn extra_closing_paren_errors() {
        assert!(evaluate("1 + 2)").is_err());
    }

    #[test]
    fn invalid_character_errors() {
        assert!(evaluate("1 + a").is_err());
    }

    #[test]
    fn trailing_operator_errors() {
        assert!(evaluate("1 +").is_err());
    }

    #[test]
    fn sats_to_btc_conversion() {
        // 100_000_000 sats = 1 BTC, at $70000/BTC = $70000
        assert!(approx(
            evaluate("100000000 / 100000000 * 70000").unwrap(),
            70000.0,
        ));
    }
}
