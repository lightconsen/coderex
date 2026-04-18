//! Welcome screen with ASCII T-Rex logo and session info.

use std::io::Write;

const T_REX: &str = r#"
                              .-.
                    ,        ( {o\
                    {\    /   ~o   }
                     \`  '-'/    ~ |
                    {.'   ~   ~  /}
                     `.    ~  ~ /
                    .'  ~  ~   `}.
                   /    ~     ~  `}
                  {      ~     ~  /)
                  |`-._  ~    ~  /
                  |    `-._     ~(
                  |        `--.__/)
                  {         `--._/
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |__|__|__|___|
                   |__|__|__|___|
                   \____________/
                    \__________/
                     \________/
"#;

/// Print the welcome screen with ASCII T-Rex logo and session information.
pub fn print_welcome_screen<W: Write>(
    out: &mut W,
    version: &str,
    model: &str,
    cwd: &str,
    api_connected: bool,
) -> std::io::Result<()> {
    let api_status = if api_connected { "connected" } else { "offline" };

    writeln!(out, "{T_REX}")?;
    writeln!(out)?;
    writeln!(out, "  CodeRex  v{version}  |  model: {model}")?;
    writeln!(out, "  cwd: {cwd}")?;
    writeln!(out, "  api: {api_status}")?;
    writeln!(out)?;
    writeln!(out, "  Type /help for commands, Ctrl+D to exit")?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_screen_renders() {
        let mut buf = Vec::new();
        print_welcome_screen(
            &mut buf,
            "0.1.0",
            "claude-sonnet-4-6",
            "/tmp/test",
            true,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("CodeRex"));
        assert!(output.contains("0.1.0"));
        assert!(output.contains("claude-sonnet-4-6"));
        assert!(output.contains("/tmp/test"));
        assert!(output.contains("api: connected"));
    }
}
