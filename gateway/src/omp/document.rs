//! Surgical YAML updates: only changed semantic leaves are rebuilt.
use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping as SemanticMapping, Value};
use std::{collections::HashMap, str::FromStr};
use yaml_edit::{
    AsYaml, Document, Mapping, MappingEntry, Sequence, SyntaxKind, YamlFile, YamlNode,
};

/// Keep the physical document separate from the semantic (possibly legacy JSON)
/// baseline. Existing documents never fall back to whole-document serialization.
pub(super) fn patch(original: Option<&[u8]>, before: &Value, after: &Value) -> Result<Vec<u8>> {
    let original = match original {
        Some(original) => original,
        None => {
            let bytes = serde_yaml::to_string(after)
                .map_err(|_| anyhow::anyhow!("无法生成 OMP 配置"))?
                .into_bytes();
            return checked(bytes, after);
        }
    };
    if before == after {
        return Ok(original.to_vec());
    }
    let physical: Value = serde_yaml::from_slice(original)
        .map_err(|_| anyhow::anyhow!("OMP 配置不是有效的 YAML，未修改文件"))?;
    // The caller treats an empty/null configuration as an empty root mapping.
    let empty_root =
        physical.is_null() && before.as_mapping().is_some_and(SemanticMapping::is_empty);
    if &physical != before && !empty_root {
        bail!("OMP 配置与编辑基线不一致，未修改文件");
    }
    let source = std::str::from_utf8(original).context("OMP 配置不是 UTF-8，未修改文件")?;
    let file = YamlFile::from_str(source)
        .map_err(|_| anyhow::anyhow!("OMP YAML 语法不支持保留格式的编辑，未修改文件"))?;
    if file.to_string().as_bytes() != original || file.documents().count() > 1 {
        bail!("OMP YAML 无法无损解析，未修改文件");
    }
    let document = file.ensure_document();
    let syntax = document
        .as_node()
        .context("OMP YAML 文档无效，未修改文件")?;
    let tree = syntax
        .ancestors()
        .last()
        .context("OMP YAML 文档无效，未修改文件")?;
    if tree
        .descendants_with_tokens()
        .any(|node| node.kind() == SyntaxKind::ERROR)
    {
        bail!("OMP YAML 包含不支持的语法，未修改文件");
    }
    let root = root(&document);
    if empty_root || root.is_none() {
        let desired = after
            .as_mapping()
            .context("OMP 配置根节点必须为对象，未修改文件")?;
        if root.is_none() {
            for (key, value) in desired {
                if !document.set(fragment(key)?, fragment(value)?) {
                    bail!("无法编辑 OMP YAML 根节点，未修改文件");
                }
            }
        } else {
            let mapping = Mapping::new();
            edit_mapping(&mapping, &SemanticMapping::new(), desired)?;
            // Replace only the explicit null node, keeping document comments,
            // directives, markers and surrounding whitespace in their CST.
            let old = root.context("OMP YAML 根节点无效，未修改文件")?;
            let old_syntax = old.as_node().context("OMP YAML 根节点无效，未修改文件")?;
            let index = old_syntax.index();
            let replacement = mapping
                .as_node()
                .context("OMP YAML 对象无效，未修改文件")?
                .clone();
            replacement.detach();
            old_syntax.detach();
            syntax.splice_children(index..index, vec![replacement.into()]);
        }
    } else if !edit(root, before, after)? {
        // OMP configurations have mapping roots. A root-type change is not a
        // reason to discard the existing physical document and serialize it.
        bail!("OMP YAML 根节点不支持此编辑，未修改文件");
    }
    checked(file.to_string().into_bytes(), after)
}

fn checked(bytes: Vec<u8>, desired: &Value) -> Result<Vec<u8>> {
    let actual: Value = serde_yaml::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("OMP YAML 编辑结果无效，未修改文件"))?;
    if &actual != desired {
        bail!("OMP YAML 编辑结果与目标不一致，未修改文件");
    }
    Ok(bytes)
}

/// Serialization is limited to a new/changed value, never the existing file.
fn fragment(value: &Value) -> Result<Document> {
    let text =
        serde_yaml::to_string(value).map_err(|_| anyhow::anyhow!("无法生成 OMP YAML 字段"))?;
    Document::from_str(&text)
        .map_err(|_| anyhow::anyhow!("OMP YAML 字段不支持保留格式的编辑，未修改文件"))
}

fn root(document: &Document) -> Option<YamlNode> {
    document
        .as_node()?
        .children()
        .find_map(YamlNode::from_syntax)
}

/// False asks the parent to replace this value, rather than its siblings.
fn edit(node: Option<YamlNode>, before: &Value, after: &Value) -> Result<bool> {
    if before == after {
        return Ok(true);
    }
    match (node, before, after) {
        (Some(YamlNode::Mapping(mapping)), Value::Mapping(before), Value::Mapping(after)) => {
            edit_mapping(&mapping, before, after)?;
            Ok(true)
        }
        (Some(YamlNode::Sequence(sequence)), Value::Sequence(before), Value::Sequence(after)) => {
            edit_sequence(&sequence, before, after)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn edit_mapping(
    mapping: &Mapping,
    before: &SemanticMapping,
    after: &SemanticMapping,
) -> Result<()> {
    // Locate entries using serde's key semantics, not the editor's scalar
    // coercions. This also rejects ambiguous keys instead of editing a neighbor.
    let mut entries: HashMap<Value, MappingEntry> = HashMap::with_capacity(mapping.len());
    for entry in mapping.entries() {
        let key = entry
            .key_node()
            .context("OMP YAML 对象键无效，未修改文件")?;
        let key: Value = serde_yaml::from_str(&key.to_string())
            .map_err(|_| anyhow::anyhow!("OMP YAML 对象键不支持编辑，未修改文件"))?;
        if !before.contains_key(&key) || entries.insert(key, entry).is_some() {
            bail!("OMP YAML 对象键存在冲突，未修改文件");
        }
    }
    if entries.len() != before.len() {
        bail!("OMP YAML 对象与编辑基线不一致，未修改文件");
    }
    for (key, previous) in before {
        let entry = entries
            .remove(key)
            .context("OMP YAML 对象键缺失，未修改文件")?;
        match after.get(key) {
            Some(desired)
                if previous != desired && !edit(entry.value_node(), previous, desired)? =>
            {
                // Mapping::set converts block fragments for flow parents;
                // MappingEntry::set_value only copies their existing CST.
                mapping.set(fragment(key)?, fragment(desired)?);
            }
            None => {
                // Remove the entry's indentation/separator too, and render an
                // emptied block mapping as {} rather than implicit null.
                mapping
                    .remove(fragment(key)?)
                    .context("无法删除 OMP YAML 对象键，未修改文件")?;
            }
            _ => {}
        }
    }
    for (key, desired) in after {
        if !before.contains_key(key) {
            let key = fragment(key)?;
            if mapping.find_entry_by_key(&key).is_some() {
                bail!("OMP YAML 新对象键与现有键冲突，未修改文件");
            }
            mapping.set(key, fragment(desired)?);
        }
    }
    Ok(())
}

fn edit_sequence(sequence: &Sequence, before: &[Value], after: &[Value]) -> Result<()> {
    if sequence.len() != before.len() {
        bail!("OMP YAML 列表与编辑基线不一致，未修改文件");
    }
    // Keep common boundaries attached. Appending/removing a model therefore
    // does not rebuild the catalog; equal-length edits recurse into each row.
    let start = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let suffix = before[start..]
        .iter()
        .rev()
        .zip(after[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_end = before.len() - suffix;
    let new_end = after.len() - suffix;
    let shared = (old_end - start).min(new_end - start);
    for index in start..start + shared {
        if !edit(sequence.get(index), &before[index], &after[index])?
            && !sequence.set(index, fragment(&after[index])?)
        {
            bail!("无法编辑 OMP YAML 列表项，未修改文件");
        }
    }
    for index in (start + shared..old_end).rev() {
        sequence
            .remove(index)
            .context("无法删除 OMP YAML 列表项，未修改文件")?;
    }
    for (offset, value) in after[start + shared..new_end].iter().enumerate() {
        sequence.insert(start + shared + offset, fragment(value)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> Value {
        serde_yaml::from_str(text).unwrap()
    }

    fn apply(source: &str, desired: &Value) -> String {
        let result = patch(Some(source.as_bytes()), &value(source), desired).unwrap();
        assert_eq!(serde_yaml::from_slice::<Value>(&result).unwrap(), *desired);
        String::from_utf8(result).unwrap()
    }

    #[test]
    fn no_change_is_byte_exact() {
        let source = b"# stream\r\n%YAML 1.2\r\n---\r\nname: 'kept' # note\r\n...\r\n# end\r\n";
        let before: Value = serde_yaml::from_slice(source).unwrap();
        assert_eq!(patch(Some(source), &before, &before).unwrap(), source);
    }

    #[test]
    fn global_leaf_edits_keep_comments_quotes_and_unrelated_whitespace() {
        let source = "# settings\r\n%YAML 1.2\r\n---\r\ncompaction:\r\n  enabled: false # switch\r\n  reserveTokens: 8192\r\n  custom: 'leave: this'  # local\r\nother: { x:  1, y: \"two\" } # untouched\r\n...\r\n# end\r\n";
        let mut desired = value(source);
        desired["compaction"]["enabled"] = Value::Bool(true);
        desired["compaction"]["reserveTokens"] = Value::Number(16384.into());
        desired["compaction"]["keepRecentTokens"] = Value::Number(20000.into());
        let result = apply(source, &desired);
        assert!(result.starts_with("# settings\r\n%YAML 1.2\r\n---\r\ncompaction:\r\n"));
        assert!(result.contains("enabled: true # switch\r\n"));
        assert!(result.contains("  custom: 'leave: this'  # local\r\n"));
        assert!(result.contains("other: { x:  1, y: \"two\" } # untouched\r\n"));
        assert!(result.ends_with("...\r\n# end\r\n"));
    }

    #[test]
    fn catalog_edits_recurse_into_one_model_and_leave_anchors_alone() {
        let source = "# catalog\ndefaults: &defaults { timeout: 30 } # anchored\ncopy: *defaults\nproviders:\n  codexpress:\n    models:\n      - id: 'first' # first model\n        contextWindow: 100000\n        maxTokens: 4096 # output\n      - id: \"second\"\n        contextWindow:  200000 # user model\n        custom: { x: 'y' }\n";
        let mut desired = value(source);
        desired["providers"]["codexpress"]["models"][0]["maxTokens"] = Value::Number(8192.into());
        let result = apply(source, &desired);
        assert!(
            result.contains("defaults: &defaults { timeout: 30 } # anchored\ncopy: *defaults\n")
        );
        assert!(
            result.contains("      - id: 'first' # first model\n        contextWindow: 100000\n")
        );
        assert!(result.contains("maxTokens: 8192 # output\n"));
        assert!(result.ends_with("      - id: \"second\"\n        contextWindow:  200000 # user model\n        custom: { x: 'y' }\n"));
    }

    #[test]
    fn catalog_insertions_and_removals_keep_surviving_rows() {
        let source = "models:\n  - id: 'first' # keep\n    custom: { x:  1 }\n  - id: obsolete\n  - id: \"last\" # keep too\n";
        let mut desired = value(source);
        desired["models"].as_sequence_mut().unwrap().remove(1);
        let result = apply(source, &desired);
        assert!(result.contains("  - id: 'first' # keep\n    custom: { x:  1 }\n"));
        assert!(result.contains("  - id: \"last\" # keep too\n"));
        desired["models"]
            .as_sequence_mut()
            .unwrap()
            .insert(1, value("id: inserted\nmaxTokens: 8192\n"));
        let result = apply(&result, &desired);
        assert!(result.contains("  - id: 'first' # keep\n    custom: { x:  1 }\n"));
        assert!(result.contains("  - id: \"last\" # keep too\n"));
    }

    #[test]
    fn missing_physical_yaml_uses_the_full_legacy_semantic_base() {
        let before = value("legacy: { custom: kept }\n");
        let mut desired = before.clone();
        desired["compaction"] = value("enabled: true\n");
        let bytes = patch(None, &before, &desired).unwrap();
        assert_eq!(serde_yaml::from_slice::<Value>(&bytes).unwrap(), desired);
        let bytes = patch(None, &before, &before).unwrap();
        assert_eq!(serde_yaml::from_slice::<Value>(&bytes).unwrap(), before);
    }

    #[test]
    fn empty_physical_documents_keep_existing_comments() {
        let before = value("{}");
        let desired = value("compaction: { enabled: true }\n");
        for source in ["", "# settings\n", "null # settings\n"] {
            let result = patch(Some(source.as_bytes()), &before, &desired).unwrap();
            assert_eq!(serde_yaml::from_slice::<Value>(&result).unwrap(), desired);
            if source.contains("# settings") {
                assert!(String::from_utf8(result).unwrap().contains("# settings"));
            }
        }
    }

    #[test]
    fn null_values_become_mappings_in_block_and_flow_parents() {
        let source = "# null containers\nmodelRoles: null # roles\ncompaction: {modelThresholds: null, custom:  'kept'} # compact\nother: { x:  1 } # untouched\n";
        let mut desired = value(source);
        desired["modelRoles"] = value("default: codexpress/model\n");
        desired["compaction"]["modelThresholds"] = value("codexpress/model: f180000\n");
        let result = apply(source, &desired);
        assert!(result.starts_with("# null containers\n"));
        assert!(result.contains("# roles\n"));
        assert!(result.contains(", custom:  'kept'} # compact\n"));
        assert!(result.ends_with("other: { x:  1 } # untouched\n"));
    }

    #[test]
    fn mapping_removals_keep_collection_layout_and_empty_mapping_semantics() {
        let source = "# settings\nstatusLine:\n  preset: 'custom' # kept\n  managed: true\n  contextLine: embedded # display\ncompaction:\n  owned: true\nlater: { untouched:  true, managed: true, last: 'kept' } # flow\ntheme:  dark # theme\n";
        let mut desired = value(source);
        desired["statusLine"]
            .as_mapping_mut()
            .unwrap()
            .remove(value("managed"));
        desired["compaction"]
            .as_mapping_mut()
            .unwrap()
            .remove(value("owned"));
        desired["later"]
            .as_mapping_mut()
            .unwrap()
            .remove(value("managed"));
        let result = apply(source, &desired);
        assert!(result.starts_with("# settings\nstatusLine:\n  preset: 'custom' # kept\n  contextLine: embedded # display\n"));
        assert!(result.contains("compaction: {}\n"));
        assert!(result.contains("later: { untouched:  true, last: 'kept' } # flow\n"));
        assert!(result.ends_with("theme:  dark # theme\n"));
    }

    #[test]
    fn stale_and_ambiguous_documents_fail_without_a_serialization_fallback() {
        let before = value("owned: old\n");
        let after = value("owned: new\n");
        assert!(patch(Some(b"owned: changed-elsewhere\n"), &before, &after).is_err());
        assert!(patch(Some(b"owned: old\nowned: old\n"), &before, &after).is_err());
        assert!(patch(
            Some(b"---\nowned: old\n---\nother: document\n"),
            &before,
            &after
        )
        .is_err());
    }
}
