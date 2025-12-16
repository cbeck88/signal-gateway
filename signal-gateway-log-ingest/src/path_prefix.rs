use globset::{Glob, GlobMatcher};

/// Trait for object which can strip prefixes from a path
pub trait PathPrefixFinder {
    /// Find the largest index such that target[..idx] matches to glob
    fn find_path_prefix(&self, target: &str) -> Option<usize>;
    /// Strip a glob pattern from a target path
    fn strip_path_prefix<'a>(&self, target: &'a str) -> &'a str {
        if let Some(idx) = self.find_path_prefix(target) {
            &target[idx..]
        } else {
            target
        }
    }
}

/// Matches a glob pattern to a path and strips matching prefix
pub struct Finder {
    inner: FinderInner,
}

impl Finder {
    /// Create a new finder for given glob pattern
    pub fn new(pattern: &str) -> Self {
        Self {
            inner: FinderInner::new(pattern),
        }
    }
}

impl PathPrefixFinder for Finder {
    fn find_path_prefix(&self, target: &str) -> Option<usize> {
        self.inner.find_path_prefix(target)
    }
}

enum FinderInner {
    LeadingDoubleStar {
        rneedle: Box<str>,
        child: Option<Box<Self>>,
    },
    MidDoubleStar {
        needle: Box<str>,
        left_child: Box<Self>,
        right_child: Box<Self>,
    },
    NoDoubleStar {
        pattern_segments: Vec<PatSegment>,
    },
}

struct PatSegment {
    segment: Box<str>,
    matcher: Option<GlobMatcher>,
}

impl FinderInner {
    fn new(pattern: &str) -> Self {
        const META: &[char] = &['*', '?', '[', ']'];

        if let Some(star2_idx) = pattern.find("**") {
            if star2_idx == 0 {
                if pattern.len() == 2 {
                    return Self::LeadingDoubleStar {
                        rneedle: Default::default(),
                        child: None,
                    };
                }
                // The character after ** must / if any
                let rem = &pattern[2..];
                assert_eq!(
                    rem.as_bytes()[0],
                    b'/',
                    "** must be followed by / or end of string"
                );
                let lits = rem.find(META).unwrap_or(rem.len());
                let rneedle = (&rem[..lits]).to_owned().into_boxed_str();
                let child = Some(Box::new(Self::new(&rem[lits..])));
                Self::LeadingDoubleStar { rneedle, child }
            } else {
                // The character before ** must be / if any
                assert_eq!(
                    pattern.as_bytes()[star2_idx - 1],
                    b'/',
                    "** must be preceeded by / or start of string"
                );
                let pref = &pattern[..star2_idx];
                let st = pref.rfind(META).map(|i| i + 1).unwrap_or(0);
                let needle = (&pref[st..]).to_owned().into_boxed_str();
                let left_child = Box::new(Self::new(&pref[..st]));
                let right_child = Box::new(Self::new(&pattern[star2_idx..]));
                Self::MidDoubleStar {
                    needle,
                    left_child,
                    right_child,
                }
            }
        } else {
            let pattern_segments = pattern
                .split('/')
                .map(|p| {
                    let matcher = if p.find(META).is_some() {
                        Some(Glob::new(p).unwrap().compile_matcher())
                    } else {
                        None
                    };
                    let segment = p.to_owned().into_boxed_str();
                    PatSegment { segment, matcher }
                })
                .collect::<Vec<_>>();

            Self::NoDoubleStar { pattern_segments }
        }
    }

    fn find_path_prefix(&self, target: &str) -> Option<usize> {
        match self {
            Self::LeadingDoubleStar { rneedle, child } => {
                let Some(child) = child.as_ref() else {
                    debug_assert!(rneedle.is_empty());
                    // pattern ** matches everything
                    return Some(target.len());
                };
                // Backtracking search on rneedle
                for (slash_idx, _) in target.rmatch_indices(&**rneedle) {
                    let target_right = slash_idx + rneedle.len();
                    if let Some(idx) = child.find_path_prefix(&target[target_right..]) {
                        return Some(idx + target_right);
                    }
                }
                None
            }
            Self::MidDoubleStar {
                needle,
                left_child,
                right_child,
            } => {
                // Backtracking search on needle
                for (needle_match_idx, _) in target.match_indices(&**needle) {
                    if let Some(_idx) = left_child.find_path_prefix(&target[..needle_match_idx]) {
                        // We found the shortest possible match for the part to the left of the **
                        // Now we need to figure out if there's any match for the right part.
                        // We start the search from ** being at the start of the string
                        let target_right = needle_match_idx + needle.len();
                        return right_child
                            .find_path_prefix(&target[target_right..])
                            .map(|ridx| ridx + target_right);
                    }
                }
                None
            }
            Self::NoDoubleStar { pattern_segments } => {
                let mut target_slashes = target.match_indices('/').map(|(idx, _)| idx);
                let mut prev_target_slash = None;

                // We're sort of doing a zip over pattern_segments.iter() and target_slashes.
                // But it can't be exactly a zip, we have to do it kind of manually to get correct
                // results.
                debug_assert!(!pattern_segments.is_empty());
                let mut pat_iter = pattern_segments.iter();
                while let Some(PatSegment { segment, matcher }) = pat_iter.next() {
                    // Compute start of this segment, excluding '/'
                    // t becomes what we would get from target.split('/')
                    let t_seg_start = prev_target_slash.map(|i| i + 1).unwrap_or(0);
                    let t = if let Some(slash_idx) = target_slashes.next() {
                        prev_target_slash = Some(slash_idx);
                        &target[t_seg_start..slash_idx]
                    } else {
                        prev_target_slash = None;
                        &target[t_seg_start..]
                    };
                    // Try to match this pattern segment against t.
                    // If there is a matcher, use that
                    if let Some(matcher) = matcher.as_ref() {
                        if !matcher.is_match(t) {
                            return None;
                        }
                    } else if !segment.is_empty() {
                        // If the segment is non-empty, but the matcher is empty,
                        // it means that we didn't bother compiling the matcher
                        // becuase there were no metacharacters, and we can just
                        // test for equality directly.
                        if *t != **segment {
                            return None;
                        }
                    } else {
                        // The case where pattern segment is empty requires special handling,
                        // because it may indicate a trailing slash, and then we don't have to match
                        // the next segment, we just match up to the slash.
                        if !t.is_empty() {
                            return if pat_iter.next().is_some() {
                                None
                            } else {
                                Some(t_seg_start)
                            };
                        }
                    }
                    // Zip structure: Check if target slashes just became exhausted
                    if prev_target_slash.is_none() {
                        return if pat_iter.next().is_some() {
                            // There's still some pattern remaining that we can't match
                            None
                        } else {
                            // They ran out at the same time, so this is a full match
                            Some(target.len())
                        };
                    }
                }
                // Pattern is exhausted before target is, so this is a partial match
                // The amount we consumed is right up to what would have been
                // prev_target_slash in the next iteration.
                // Note: prev_target_slash is not None in this path, because
                // we always enter the loop at least once -- split on an empty string
                // still produces an empty string, and so pattern_segments is not empty.
                prev_target_slash
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_simple_glob(pattern: &str, target: &str) -> bool {
        Finder::new(pattern).find_path_prefix(target).is_some()
    }

    // Strip a glob pattern from a target path
    fn strip_path_prefix<'a>(pattern: &'_ str, target: &'a str) -> &'a str {
        if let Some(idx) = Finder::new(pattern).find_path_prefix(target) {
            &target[idx..]
        } else {
            target
        }
    }

    #[test]
    fn test_match_simple_glob() {
        assert!(!match_simple_glob("", "foo"));
        assert!(!match_simple_glob("f", "foo"));
        assert!(!match_simple_glob("o", "foo"));
        assert!(!match_simple_glob("fo", "foo"));
        assert!(match_simple_glob("foo", "foo"));
        assert!(match_simple_glob("*o", "foo"));
        assert!(!match_simple_glob("*f", "foo"));
        assert!(!match_simple_glob("o*", "foo"));
        assert!(match_simple_glob("f*", "foo"));
        assert!(match_simple_glob("*o*", "foo"));
        assert!(match_simple_glob("*oo", "foo"));
    }

    #[test]
    fn test_strip_path_prefix() {
        assert_eq!(strip_path_prefix("foo", "foo"), "");
        assert_eq!(strip_path_prefix("f/o", "f/o/o"), "/o");
        assert_eq!(strip_path_prefix("f/", "f/o/o"), "o/o");
        assert_eq!(strip_path_prefix("/f", "/f/o/o"), "/o/o");
        assert_eq!(strip_path_prefix("*/", "/foo"), "foo");
        assert_eq!(strip_path_prefix("/*", "/foo"), "");
        assert_eq!(strip_path_prefix("/*", "/foo/bar"), "/bar");
        assert_eq!(strip_path_prefix("/*/", "/foo/bar"), "bar");
        assert_eq!(strip_path_prefix("/home/*/", "/home/chris/foo"), "foo");
        assert_eq!(
            strip_path_prefix("**/target", "/home/chris/foo/target/a"),
            "/a"
        );
        assert_eq!(
            strip_path_prefix("/**/target/*/*/", "/home/chris/foo"),
            "/home/chris/foo"
        );
        assert_eq!(
            strip_path_prefix("/**/target/*/*/", "/home/chris/foo/target"),
            "/home/chris/foo/target"
        );
        assert_eq!(
            strip_path_prefix("/**/target/*/*/", "/home/chris/foo/target/a/b/c"),
            "c"
        );
        assert_eq!(
            strip_path_prefix("a/**/target", "a//home/chris/foo/target/b"),
            "/b"
        );
    }

    #[test]
    fn test_finder_strip_path_prefix() {
        {
            let finder = Finder::new("foo");
            assert_eq!(finder.strip_path_prefix("foo"), "");
            assert_eq!(finder.strip_path_prefix("fo"), "fo");
            assert_eq!(finder.strip_path_prefix("fooo"), "fooo");
            assert_eq!(finder.strip_path_prefix("/foo"), "/foo");
            assert_eq!(finder.strip_path_prefix("foo/"), "/");
            assert_eq!(finder.strip_path_prefix("foo/foo"), "/foo");
            assert_eq!(finder.strip_path_prefix("foo/foo/"), "/foo/");
        }
        {
            let finder = Finder::new("/foo");
            assert_eq!(finder.strip_path_prefix("/foo"), "");
            assert_eq!(finder.strip_path_prefix("/fo"), "/fo");
            assert_eq!(finder.strip_path_prefix("/fooo"), "/fooo");
            assert_eq!(finder.strip_path_prefix("/foo/"), "/");
            assert_eq!(finder.strip_path_prefix("/foo/foo"), "/foo");
            assert_eq!(finder.strip_path_prefix("/foo/foo/"), "/foo/");
        }
        {
            let finder = Finder::new("f/o");
            assert_eq!(finder.strip_path_prefix("f/o/o"), "/o");
            assert_eq!(finder.strip_path_prefix("f/o/o/"), "/o/");
            assert_eq!(finder.strip_path_prefix("/f/o/o"), "/f/o/o");
            assert_eq!(finder.strip_path_prefix("fo"), "fo");
            assert_eq!(finder.strip_path_prefix("fooo"), "fooo");
            assert_eq!(finder.strip_path_prefix("f/oo"), "f/oo");
        }
        {
            let finder = Finder::new("f/");
            assert_eq!(finder.strip_path_prefix("f/o/o"), "o/o");
            assert_eq!(finder.strip_path_prefix("/f/o/o"), "/f/o/o");
            assert_eq!(finder.strip_path_prefix("fo"), "fo");
            assert_eq!(finder.strip_path_prefix("fo/"), "fo/");
        }
        {
            let finder = Finder::new("/f");
            assert_eq!(finder.strip_path_prefix("/f/o/o"), "/o/o");
            assert_eq!(finder.strip_path_prefix("/f/o/"), "/o/");
            assert_eq!(finder.strip_path_prefix("f/o/"), "f/o/");
            assert_eq!(finder.strip_path_prefix("/fo"), "/fo");
            assert_eq!(finder.strip_path_prefix("/fo/"), "/fo/");
        }
        {
            let finder = Finder::new("*/");
            assert_eq!(finder.strip_path_prefix("/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("a/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("ab/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("foo/foo/"), "foo/");
        }
        {
            let finder = Finder::new("/*");
            assert_eq!(finder.strip_path_prefix("/foo"), "");
            assert_eq!(finder.strip_path_prefix("a/foo"), "a/foo");
            assert_eq!(finder.strip_path_prefix("/foo/"), "/");
            assert_eq!(finder.strip_path_prefix("a/foo/"), "a/foo/");
            assert_eq!(finder.strip_path_prefix("/foo/bar"), "/bar");
        }
        {
            let finder = Finder::new("/*/");
            assert_eq!(finder.strip_path_prefix("/foo"), "/foo");
            assert_eq!(finder.strip_path_prefix("/foo/"), "");
            assert_eq!(finder.strip_path_prefix("a/foo/"), "a/foo/");
            assert_eq!(finder.strip_path_prefix("/foo/bar"), "bar");
            assert_eq!(finder.strip_path_prefix("/foo/bar/"), "bar/");
        }
        {
            let finder = Finder::new("/home/*/");
            assert_eq!(finder.strip_path_prefix("/home/chris/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("/home/chris/foo/"), "foo/");
            assert_eq!(finder.strip_path_prefix("home/chris/foo"), "home/chris/foo");
            assert_eq!(finder.strip_path_prefix("/foo/"), "/foo/");
            assert_eq!(
                finder.strip_path_prefix("/foo/home/chris/"),
                "/foo/home/chris/"
            );
        }
        {
            let finder = Finder::new("**/target");
            assert_eq!(finder.strip_path_prefix("/target"), "");
            assert_eq!(finder.strip_path_prefix("target"), "target");
            assert_eq!(finder.strip_path_prefix("/target/"), "/");
            assert_eq!(finder.strip_path_prefix("/target/target/"), "/");
            assert_eq!(finder.strip_path_prefix("/target/target"), "");
            assert_eq!(finder.strip_path_prefix("target/target"), "");
            assert_eq!(finder.strip_path_prefix("/target/target/"), "/");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/"), "/");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/foo"), "/foo");
        }
        {
            let finder = Finder::new("**/target/");
            assert_eq!(finder.strip_path_prefix("/target"), "/target");
            assert_eq!(finder.strip_path_prefix("target"), "target");
            assert_eq!(finder.strip_path_prefix("/target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/target"), "target");
            assert_eq!(finder.strip_path_prefix("target/target"), "target/target");
            assert_eq!(finder.strip_path_prefix("/target/target/a"), "a");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("target/foo/target/foo"), "foo");
        }
        {
            let finder = Finder::new("/**/target/");
            assert_eq!(finder.strip_path_prefix("/target"), "/target");
            assert_eq!(finder.strip_path_prefix("//target"), "//target");
            assert_eq!(finder.strip_path_prefix("target"), "target");
            assert_eq!(finder.strip_path_prefix("/target/"), "/target/");
            assert_eq!(finder.strip_path_prefix("//target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/target"), "/target/target");
            assert_eq!(finder.strip_path_prefix("target/target"), "target/target");
            assert_eq!(finder.strip_path_prefix("/target/target/a"), "a");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/"), "");
            assert_eq!(finder.strip_path_prefix("/target/foo/target/foo"), "foo");
            assert_eq!(finder.strip_path_prefix("target/foo/target/foo"), "foo");
        }

        {
            let finder = Finder::new("/**/target/*/");
            assert_eq!(finder.strip_path_prefix("/target"), "/target");
            assert_eq!(finder.strip_path_prefix("//target"), "//target");
            assert_eq!(finder.strip_path_prefix("//target/"), "//target/");
            assert_eq!(finder.strip_path_prefix("//target//"), "");
            assert_eq!(finder.strip_path_prefix("//target/a/"), "");
            assert_eq!(finder.strip_path_prefix("//target/a/b"), "b");
            assert_eq!(finder.strip_path_prefix("//target/a/b/"), "b/");
            assert_eq!(finder.strip_path_prefix("//target/a/target"), "target");
            assert_eq!(finder.strip_path_prefix("//target/a/target/"), "target/");
            assert_eq!(finder.strip_path_prefix("/target/target/a/b"), "b");
            assert_eq!(
                finder.strip_path_prefix("/target/target/a/target"),
                "target"
            );
            assert_eq!(finder.strip_path_prefix("/target/target/a/b/"), "b/");
            assert_eq!(
                finder.strip_path_prefix("/target/target/a/target/"),
                "target/"
            );
        }
    }
}
