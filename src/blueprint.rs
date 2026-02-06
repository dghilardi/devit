use anyhow::Result;
use regex::Regex;
use similar::{ChangeTag, TextDiff};
use console::style;

pub struct Blueprint;

impl Blueprint {

    /// Modifies the image tag in the YAML content while preserving formatting/comments.
    /// It searches for 'image: ...:<old_tag>' and replaces it.
    /// Modifies the image tag in the YAML content while preserving formatting/comments.
    /// It searches for 'image: <base_image>:<old_tag>' and replaces it.
    pub fn update_image_tag(content: &str, base_image: &str, new_tag: &str) -> Result<String> {
        // Escape the base_image for regex safety
        let escaped_base = regex::escape(base_image);
        
        // Pattern: image: <escaped_base_image> : (anything that looks like a tag)
        // We look for the line that starts with 'image:' and contains our specific base_image.
        let pattern = format!(r"(?m)^(\s*image:\s*{})[:@][^\s#]+", escaped_base);
        let re = Regex::new(&pattern).unwrap();
        
        if !re.is_match(content) {
            return Err(anyhow::anyhow!("Could not find 'image: {}' field in the YAML content", base_image));
        }

        let new_content = re.replace_all(content, format!("$1:{}", new_tag)).to_string();
        Ok(new_content)
    }

    /// Displays a colored diff between old and new content.
    pub fn show_diff(old: &str, new: &str, filename: &str, unified: bool) {
        println!("\n{} {}", style("---").dim(), style(filename).bold());
        println!("{} {}", style("+++").dim(), style(filename).bold());

        let diff = TextDiff::from_lines(old, new);

        if unified {
            for group in diff.grouped_ops(3) {
                for op in group {
                    match op {
                        similar::DiffOp::Equal { old_index, len, .. } => {
                            for line in &diff.old_slices()[old_index..old_index + len] {
                                print!(" {}", style(line).dim());
                            }
                        }
                        similar::DiffOp::Delete { old_index, old_len, .. } => {
                            for line in &diff.old_slices()[old_index..old_index + old_len] {
                                print!("-{}", style(line).red());
                            }
                        }
                        similar::DiffOp::Insert { new_index, new_len, .. } => {
                            for line in &diff.new_slices()[new_index..new_index + new_len] {
                                print!("+{}", style(line).green());
                            }
                        }
                        similar::DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                            for line in &diff.old_slices()[old_index..old_index + old_len] {
                                print!("-{}", style(line).red());
                            }
                            for line in &diff.new_slices()[new_index..new_index + new_len] {
                                print!("+{}", style(line).green());
                            }
                        }
                    }
                }
                println!("{}", style("@@ ... @@").cyan());
            }
        } else {
            for change in diff.iter_all_changes() {
                let (sign, color) = match change.tag() {
                    ChangeTag::Delete => ("-", "red"),
                    ChangeTag::Insert => ("+", "green"),
                    ChangeTag::Equal => (" ", "white"),
                };
                
                let line = change.to_string();
                let styled_line = if color == "red" {
                    style(format!("{}{}", sign, line)).red()
                } else if color == "green" {
                    style(format!("{}{}", sign, line)).green()
                } else {
                    style(format!("{}{}", sign, line)).dim()
                };

                print!("{}", styled_line);
            }
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_image_tag_with_sidecars() {
        let content = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
      - name: main
        image: gcr.io/my-project/my-app:v1
      - name: sidecar
        image: haproxy:2.4
"#;
        let base_image = "gcr.io/my-project/my-app";
        let new_tag = "v2";
        let updated = Blueprint::update_image_tag(content, base_image, new_tag).unwrap();
        
        assert!(updated.contains("image: gcr.io/my-project/my-app:v2"));
        assert!(updated.contains("image: haproxy:2.4"));
    }
}
