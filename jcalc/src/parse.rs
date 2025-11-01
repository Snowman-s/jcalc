#[derive(Debug, PartialEq, Eq)]
pub enum Expression {
  Number(i64),
  Binary(Operator),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Operator {
  Add,
  Subtract,
  Multiply,
  Divide,
}

pub fn parse_input(input: &str) -> Result<Vec<Expression>, String> {
  // パース処理
  let mut exprs = Vec::new();
  let remain = parse_expression(input, &mut exprs)?;
  if !remain.trim().is_empty() {
    return Err(format!("Unexpected input remaining: '{}'", remain));
  }
  Ok(exprs)
}

pub fn parse_expression(input: &str, exprs: &mut Vec<Expression>) -> Result<String, String> {
  parse_add_sub(input, exprs)
}

// + - のレベル
pub fn parse_add_sub(input: &str, exprs: &mut Vec<Expression>) -> Result<String, String> {
  let mut rest;

  // 最初の項（* / レベル）をパース
  rest = parse_mul_div(input, exprs)?;

  loop {
    let rest_trimmed = rest.trim_start();
    if rest_trimmed.starts_with('+') || rest_trimmed.starts_with('-') {
      let op = if rest_trimmed.starts_with('+') {
        Operator::Add
      } else {
        Operator::Subtract
      };
      let next_input = &rest_trimmed[1..];
      rest = parse_mul_div(next_input, exprs)?;
      exprs.push(Expression::Binary(op));
    } else {
      break;
    }
  }

  Ok(rest)
}

// * / のレベル
pub fn parse_mul_div(input: &str, exprs: &mut Vec<Expression>) -> Result<String, String> {
  let mut rest;

  // 最初の項（数字または括弧）をパース
  rest = parse_primary(input, exprs)?;

  loop {
    let rest_trimmed = rest.trim_start();
    if rest_trimmed.starts_with('*') || rest_trimmed.starts_with('/') {
      let op = if rest_trimmed.starts_with('*') {
        Operator::Multiply
      } else {
        Operator::Divide
      };
      let next_input = &rest_trimmed[1..];
      rest = parse_primary(next_input, exprs)?;
      exprs.push(Expression::Binary(op));
    } else {
      break;
    }
  }

  Ok(rest)
}

// 数字や括弧をパース
pub fn parse_primary(input: &str, exprs: &mut Vec<Expression>) -> Result<String, String> {
  let s = input.trim_start();
  if let Some(after_paren) = s.strip_prefix('(') {
    let rest = parse_expression(after_paren, exprs)?;
    let rest = rest.trim_start();
    if let Some(remaining) = rest.strip_prefix(')') {
      Ok(remaining.to_string())
    } else {
      Err("Expected ')'".to_string())
    }
  } else {
    // 数字のパース
    let chars = s.chars();
    let mut i = 0;
    for c in chars {
      if c.is_ascii_digit() {
        i += c.len_utf8();
      } else {
        break;
      }
    }
    if i == 0 {
      return Err(format!("Expected number at '{}'", s));
    }
    let num_str = &s[..i];
    let rest = &s[i..];
    let num: i64 = num_str.parse().map_err(|_| "Invalid number")?;
    exprs.push(Expression::Number(num));
    Ok(rest.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_expression() {
    let input = "3 + 5 * (2 - 8)";
    let result = parse_input(input);
    assert_eq!(
      result,
      Ok(vec![
        Expression::Number(3),
        Expression::Number(5),
        Expression::Number(2),
        Expression::Number(8),
        Expression::Binary(Operator::Subtract),
        Expression::Binary(Operator::Multiply),
        Expression::Binary(Operator::Add),
      ])
    );
  }
}
