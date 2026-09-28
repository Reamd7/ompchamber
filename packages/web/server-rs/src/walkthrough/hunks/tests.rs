//! 从 hunks.test.js 移植的解析契约测试：文件与 hunk 拆分、id 稳定性、
//! 跨 scope 隔离、字节级重复去重、状态与二进制标记、索引解析。
//! Tests ported from `server/lib/walkthrough/hunks.test.js`.

use super::{index_hunks, list_hunk_ids, parse_diff_files};

/// 标准测试 diff：两个文件共三个 hunk，第二个文件为新增。
const TWO_FILE_DIFF: &str = "diff --git a/src/a.ts b/src/a.ts
index 1111111..2222222 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,3 +1,4 @@
 const a = 1;
+const b = 2;
 const c = 3;
 const d = 4;
@@ -20,2 +21,2 @@
-const old = true;
+const next = true;
diff --git a/src/b.ts b/src/b.ts
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/b.ts
@@ -0,0 +1,2 @@
+export const x = 1;
+export const y = 2;
";

/// 文件与 hunk 正确拆分：行区间、增删计数与新增文件的状态。
#[test]
fn splits_files_and_hunks_with_line_ranges_and_counts() {
    let files = parse_diff_files(TWO_FILE_DIFF, "working");

    assert_eq!(
        files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["src/a.ts", "src/b.ts"]
    );
    assert_eq!(files[0].hunks.len(), 2);
    assert_eq!(
        (files[0].hunks[0].old_start, files[0].hunks[0].old_lines),
        (1, 3)
    );
    assert_eq!(
        (files[0].hunks[0].new_start, files[0].hunks[0].new_lines),
        (1, 4)
    );
    assert_eq!((files[0].hunks[0].added, files[0].hunks[0].deleted), (1, 0));
    assert_eq!((files[0].hunks[1].added, files[0].hunks[1].deleted), (1, 1));
    assert_eq!(files[1].status, "added");
    assert_eq!((files[1].hunks[0].added, files[1].hunks[0].deleted), (2, 0));
}

/// 每个 hunk 的 patch 可独立应用：带文件头且只含这一个 hunk。
#[test]
fn produces_a_standalone_applicable_patch_per_hunk() {
    let files = parse_diff_files(TWO_FILE_DIFF, "working");
    let patch = &files[0].hunks[1].patch;

    assert!(patch.starts_with("diff --git a/src/a.ts b/src/a.ts"));
    assert!(patch.contains("--- a/src/a.ts"));
    assert!(patch.contains("+++ b/src/a.ts"));
    assert_eq!(
        patch
            .match_indices("@@")
            .filter(|(at, _)| *at == 0 || patch[..*at].ends_with('\n'))
            .count(),
        1
    );
    assert!(patch.contains("+const next = true;"));
    assert!(!patch.contains("const b = 2;"));
}

/// 相同输入重复解析得到相同且唯一的 id 列表。
#[test]
fn keeps_ids_stable_across_reparses_of_identical_input() {
    let first = list_hunk_ids(&parse_diff_files(TWO_FILE_DIFF, "working"));
    let second = list_hunk_ids(&parse_diff_files(TWO_FILE_DIFF, "working"));

    assert_eq!(first, second);
    let unique: std::collections::HashSet<_> = first.iter().collect();
    assert_eq!(unique.len(), first.len());
}

/// 邻近 hunk 被编辑时，未改动的 hunk 保持原 id 不变。
#[test]
fn keeps_an_untouched_hunk_addressable_when_a_neighbour_changes() {
    let before = &parse_diff_files(TWO_FILE_DIFF, "working")[0].hunks;
    let edited = TWO_FILE_DIFF.replace("+const next = true;", "+const next = false;");
    let after = &parse_diff_files(&edited, "working")[0].hunks;

    // The unrelated first hunk survives; only the edited one loses its id.
    assert_eq!(after[0].id, before[0].id);
    assert_ne!(after[1].id, before[1].id);
}

/// 不同 scope 中的相同 hunk 得到不同的 id。
#[test]
fn separates_identical_hunks_in_different_scopes() {
    let staged = &parse_diff_files(TWO_FILE_DIFF, "staged")[0].hunks[0].id;
    let working = &parse_diff_files(TWO_FILE_DIFF, "working")[0].hunks[0].id;

    assert_ne!(staged, working);
}

/// 同一文件内字节级重复的 hunk 通过后缀区分，id 仍然唯一。
#[test]
fn disambiguates_byte_identical_hunks_inside_one_file() {
    let repeated = "diff --git a/src/dup.ts b/src/dup.ts
--- a/src/dup.ts
+++ b/src/dup.ts
@@ -1,1 +1,2 @@
+import { thing } from './thing';
@@ -1,1 +1,2 @@
+import { thing } from './thing';
";

    let ids = list_hunk_ids(&parse_diff_files(repeated, "working"));

    assert_eq!(ids.len(), 2);
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 2);
}

/// rename 与 delete 的状态、新旧路径被正确记录。
#[test]
fn records_renames_and_deletions() {
    let renamed = "diff --git a/src/old.ts b/src/new.ts
similarity index 90%
rename from src/old.ts
rename to src/new.ts
--- a/src/old.ts
+++ b/src/new.ts
@@ -1,1 +1,1 @@
-const a = 1;
+const a = 2;
diff --git a/src/gone.ts b/src/gone.ts
deleted file mode 100644
--- a/src/gone.ts
+++ /dev/null
@@ -1,1 +0,0 @@
-const gone = true;
";

    let files = parse_diff_files(renamed, "working");

    assert_eq!(files[0].path, "src/new.ts");
    assert_eq!(files[0].old_path.as_deref(), Some("src/old.ts"));
    assert_eq!(files[0].status, "renamed");
    assert_eq!(files[1].path, "src/gone.ts");
    assert_eq!(files[1].status, "deleted");
}

/// 二进制文件被标记且不产生任何 hunk。
#[test]
fn marks_binary_files_and_gives_them_no_hunks() {
    let binary = "diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
";

    let files = parse_diff_files(binary, "working");

    assert_eq!(files[0].path, "logo.png");
    assert!(files[0].binary);
    assert!(files[0].hunks.is_empty());
}

/// 空输入与纯空白输入都解析为空列表。
#[test]
fn returns_nothing_for_empty_or_whitespace_input() {
    assert!(parse_diff_files("", "working").is_empty());
    assert!(parse_diff_files("   \n", "working").is_empty());
}

/// 带引号的含空格路径，两侧都能正确解析。
#[test]
fn parses_quoted_paths_containing_spaces() {
    let quoted = "diff --git \"a/old name.ts\" \"b/new name.ts\"
--- \"a/old name.ts\"
+++ \"b/new name.ts\"
@@ -1,1 +1,1 @@
-a
+b
";
    let files = parse_diff_files(quoted, "working");
    assert_eq!(files[0].path, "new name.ts");
    assert_eq!(files[0].old_path.as_deref(), Some("old name.ts"));
}

/// 省略计数的 hunk 头按 1 解析，头行原文保留。
#[test]
fn parses_hunk_headers_with_default_counts_and_context() {
    let patch = "diff --git a/x b/x
--- a/x
+++ b/x
@@ -7 +7,2 @@
-a
+b
+c
";
    let files = parse_diff_files(patch, "s");
    let hunk = &files[0].hunks[0];
    assert_eq!(hunk.old_lines, 1);
    assert_eq!(hunk.new_lines, 2);
    assert_eq!(hunk.header, "@@ -7 +7,2 @@");
}

/// 首个文件头之前的杂行被忽略，不影响解析。
#[test]
fn ignores_leading_junk_before_the_first_file_header() {
    let patch = "some trailing noise\ndiff --git a/x b/x
--- a/x
+++ b/x
@@ -1,1 +1,2 @@
+a
";
    let files = parse_diff_files(patch, "s");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "x");
}

/// 索引中每个 id 都能解析回带所属文件路径的 hunk。
#[test]
fn index_hunks_maps_every_id_to_its_hunk_with_the_owning_file_path() {
    let files = parse_diff_files(TWO_FILE_DIFF, "working");
    let index = index_hunks(&files);

    assert_eq!(index.len(), 3);
    for id in index.ids() {
        let hunk = index.get(id).expect("id resolves");
        assert_eq!(hunk.id, id);
        assert!(!hunk.path.is_empty());
    }
}
