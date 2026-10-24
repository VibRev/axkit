//! A tool's agent skill, bundled at build time and handed out as text
//! (CONVENTIONS §9).
//!
//! The tool does not install it: every harness loads skills from a
//! directory of its own, and those move between releases. The agent puts
//! the files where its harness looks, and compares the frontmatter's
//! `metadata.version` with `<tool> --version` to tell a stale copy.

/// One file of a skill, at a path relative to the skill's directory.
#[derive(Clone, Copy, Debug)]
pub struct SkillFile {
    pub path: &'static str,
    pub text: &'static str,
}

/// A skill as a tool bundles it: `files` holds `SKILL.md` and any
/// reference files it links to.
#[derive(Clone, Copy, Debug)]
pub struct Skill {
    /// The skill's directory name, also its frontmatter `name`.
    pub name: &'static str,
    pub files: &'static [SkillFile],
}

impl Skill {
    /// The `SKILL.md` text.
    ///
    /// # Panics
    ///
    /// The skill has no `SKILL.md`: the tool bundled it wrong.
    pub fn skill_md(&self) -> &'static str {
        self.file("SKILL.md").map(|f| f.text).expect("a bundled skill has a SKILL.md")
    }

    /// The bundled file at `path`, relative to the skill's directory.
    pub fn file(&self, path: &str) -> Option<&'static SkillFile> {
        self.files.iter().find(|f| f.path == path)
    }

    /// The frontmatter's `metadata.version`, quotes taken off.
    pub fn version(&self) -> Option<&'static str> {
        let rest = self.skill_md().strip_prefix("---\n")?;
        let (front, _) = rest.split_once("\n---")?;
        let mut in_metadata = false;
        for line in front.lines() {
            if !line.starts_with([' ', '\t']) {
                in_metadata = line.trim_end() == "metadata:";
            } else if in_metadata && let Some(value) = line.trim_start().strip_prefix("version:") {
                return Some(value.trim().trim_matches(['"', '\'']));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SKILL: Skill = Skill {
        name: "t",
        files: &[
            SkillFile {
                path: "SKILL.md",
                text: "---\nname: t\ndescription: d\nmetadata:\n  author: x\n  version: \"1.2.0\"\n---\nversion: not this\n",
            },
            SkillFile { path: "references/more.md", text: "more\n" },
        ],
    };

    #[test]
    fn files_by_path() {
        assert!(SKILL.skill_md().starts_with("---\nname: t\n"));
        assert_eq!(SKILL.file("references/more.md").map(|f| f.text), Some("more\n"));
        assert!(SKILL.file("nope.md").is_none());
    }

    #[test]
    fn the_version_comes_from_the_frontmatter_metadata() {
        assert_eq!(SKILL.version(), Some("1.2.0"));
        static TOP_LEVEL: Skill = Skill { name: "t", files: &[SkillFile { path: "SKILL.md", text: "---\nname: t\nversion: 9\n---\n" }] };
        assert_eq!(TOP_LEVEL.version(), None, "only metadata.version counts");
        static NONE: Skill = Skill { name: "t", files: &[SkillFile { path: "SKILL.md", text: "no frontmatter\n" }] };
        assert_eq!(NONE.version(), None);
    }
}
