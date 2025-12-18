use globset::{Glob, GlobMatcher};

/// Helper which removes prefixes (represented as a glob) from a file path,
/// trying each one in sequence.
///
/// For example, with the globset:
/// [ "/home/*/", ".cargo/registry/src/*/" ]
///
/// this can remove a lot of noise if you build from your home directory with
/// default cargo settings, regardless of what your username is etc.
pub struct StripPathPrefixes {
    finders: Vec<Finder>,
}

impl StripPathPrefixes {
    pub fn new<'a, T>(iter: T) -> Self
    where
        T: Iterator<Item = &'a str>,
    {
        Self {
            finders: iter.map(Finder::new).collect(),
        }
    }

    pub fn strip_path_prefix<'a>(&self, mut target: &'a str) -> &'a str {
        for f in self.finders.iter() {
            target = f.strip_path_prefix(target);
        }
        target
    }
}

impl<A: AsRef<str>> FromIterator<A> for StripPathPrefixes {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = A>,
    {
        let finders = iter.into_iter().map(|s| Finder::new(s.as_ref())).collect();
        Self { finders }
    }
}

/// Trait for object which can strip prefixes from a path
trait PathPrefixFinder {
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
#[derive(Clone)]
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

#[allow(clippy::enum_variant_names)]
#[derive(Clone)]
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
        globs: Vec<GlobSegment>,
        last: Box<str>,
    },
}

#[derive(Clone)]
struct GlobSegment {
    prefix: Box<str>,
    matcher: GlobMatcher,
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
                let rneedle = rem[..lits].to_owned().into_boxed_str();
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
                let needle = pref[st..].to_owned().into_boxed_str();
                let left_child = Box::new(Self::new(&pref[..st]));
                let right_child = Box::new(Self::new(&pattern[star2_idx..]));
                Self::MidDoubleStar {
                    needle,
                    left_child,
                    right_child,
                }
            }
        } else {
            let mut globs = vec![];
            let mut working = String::new();

            // Split pattern in `/` separated segments, then see which ones actually
            // have metacharacters.
            // The final structure will be literal - glob - literal - glob - literal
            // The current "literal" is buffered into "working", and is allowed to contain /,
            // while glob can never contain /.
            // Each time the segment doesn't contain meta, we add it to working.
            // If it does contain meta, then the entire working buffer becomes
            // the prefix for this glob, and the current segment is compiled as a blob.
            // At the end, any remaining working buffer becomes the "last" literal.
            let mut first = true;
            for segment in pattern.split('/') {
                if first {
                    first = false;
                } else {
                    working.push('/');
                }
                if segment.find(META).is_some() {
                    let prefix = core::mem::take(&mut working).into_boxed_str();
                    let matcher = Glob::new(segment).unwrap().compile_matcher();
                    globs.push(GlobSegment { prefix, matcher });
                } else {
                    working += segment;
                }
            }
            let last = working.into_boxed_str();

            Self::NoDoubleStar { globs, last }
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
            Self::NoDoubleStar { globs, last } => {
                let mut target_remaining = target;
                // As long as there are glob segments, strip off their prefix (or hard fail)
                // and try to match the glob on the remainder, always requiring to match up
                // to next / if there is one.
                for GlobSegment { prefix, matcher } in globs.iter() {
                    debug_assert!(prefix.is_empty() || prefix.ends_with('/'));
                    let rem = target_remaining.strip_prefix(&**prefix)?;
                    if let Some(slash_idx) = rem.find('/') {
                        if !matcher.is_match(&rem[..slash_idx]) {
                            return None;
                        }
                        target_remaining = &rem[slash_idx..];
                    } else {
                        if !matcher.is_match(rem) {
                            return None;
                        }
                        target_remaining = "";
                    }
                }
                // If there is a last literal, then strip it off.
                //
                // There is an edge-case -- the last literal is not allowed
                // to partially match a path component at the end.
                //
                // If last doesn't end with '/', and the remaining unmatched stuff is non-empty
                // and doesn't start with '/', then we know that we broke a path component in the middle,
                // so we should return None. Otherwise we compute the length of what we matched
                // by subtracting remaining string length from starting string length.
                if !last.is_empty() {
                    target_remaining = target_remaining.strip_prefix(&**last)?;
                    if !target_remaining.is_empty()
                        && !target_remaining.starts_with('/')
                        && !last.ends_with('/')
                    {
                        // not allowed to partial-match a path component at the end
                        return None;
                    }
                }
                Some(target.len() - target_remaining.len())
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
        // By convention, we say that empty pattern matches everything
        assert!(match_simple_glob("", "foo"));
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

        {
            let finder = Finder::new("*a/**/b*");
            assert_eq!(finder.strip_path_prefix("a/b"), "a/b");
            assert_eq!(finder.strip_path_prefix("a//b"), "");
            assert_eq!(finder.strip_path_prefix("a/c/b"), "");
            assert_eq!(finder.strip_path_prefix("ca/c/b"), "");
            assert_eq!(finder.strip_path_prefix("ca/c/bc"), "");
            assert_eq!(finder.strip_path_prefix("ca/c/dbc"), "ca/c/dbc");
            assert_eq!(finder.strip_path_prefix("ca/c/b/dbc"), "/dbc");
        }
    }
}
