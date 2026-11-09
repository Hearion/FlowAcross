//! 生成用管理员权限执行的安装命令（macOS 低延迟模式的后台服务）。
//!
//! 文件内容直接以 base64 写在 root 命令里，由 root 解码后写进只有 root 能写的目录，再原子改名就位。
//! 不能先把文件写到用户可写的临时目录、再让 root 去拷贝：授权弹窗停在屏幕上的这段时间里，
//! 同一用户下的任何进程都能把那个文件换掉，换来以 root 身份常驻执行。

/// 单引号包住，供 shell 使用（出现单引号时按标准方式转义）。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 标准 base64（带 `=` 补齐），输出只含 `A-Za-z0-9+/=`，放进 shell 单引号里不需要转义。
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 以 root 身份把 `content` 写到 `dest`（属主 root:wheel，权限 `mode`）。
/// 先写到同目录下的临时名再改名，读到的永远是完整文件；目标目录必须只有 root 可写。
pub fn write_file(dest: &str, content: &str, mode: &str) -> String {
    let tmp = format!("{dest}.new");
    format!(
        "rm -f {tmp} && printf %s {data} | /usr/bin/base64 -D > {tmp} && \
         chown root:wheel {tmp} && chmod {mode} {tmp} && mv -f {tmp} {dest}",
        tmp = sh_quote(&tmp),
        data = sh_quote(&base64(content.as_bytes())),
        dest = sh_quote(dest),
    )
}

/// 以 root 身份确保 `dir` 是 root 所有、只有 root 可写的真实目录（不是符号链接）。
pub fn ensure_root_dir(dir: &str) -> String {
    let d = sh_quote(dir);
    format!("[ ! -L {d} ] && mkdir -p {d} && chown root:wheel {d} && chmod 755 {d}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        assert_eq!(base64("流界".as_bytes()), "5rWB55WM");
    }

    #[test]
    fn quoting() {
        assert_eq!(
            sh_quote("/Library/Application Support/x"),
            "'/Library/Application Support/x'"
        );
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn write_file_embeds_content_and_never_reads_user_files() {
        let script = "#!/bin/sh\necho 'a \"b\"' $HOME `x`\n";
        let cmd = write_file(
            "/Library/Application Support/FlowAcross/g.sh",
            script,
            "755",
        );
        // 内容只以 base64 出现：不会被 shell 展开，也不依赖任何用户可写的文件。
        assert!(cmd.contains(&base64(script.as_bytes())));
        assert!(!cmd.contains("$HOME") && !cmd.contains('`'));
        assert!(!cmd.to_lowercase().contains("tmp/") && !cmd.contains("TMPDIR"));
        // 先写临时名再改名就位。
        assert!(cmd.contains("mv -f '/Library/Application Support/FlowAcross/g.sh.new' '/Library/Application Support/FlowAcross/g.sh'"));
        assert!(cmd.contains("chmod 755"));
        assert!(ensure_root_dir("/Library/x").starts_with("[ ! -L '/Library/x' ]"));
    }
}
