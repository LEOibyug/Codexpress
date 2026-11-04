import XCTest
@testable import Codexpress

final class OMPConfigurationTests: XCTestCase {
    func testOpenAIPresetRemovesShadowingRulesWithoutInventingUnknownModelCapacity() throws {
        let stored = try JSONDecoder().decode(JSONValue.self, from: Data(#"""
        {"compaction":{"thresholdTokens":10000,"modelThresholds":{"codexpress/*":"f20000","codexpress/unknown":"f30000","personal/keep":"75%"}},
         "providerRemoteCompaction":{"endpoint":"https://old.invalid/compact","model":"old-summary"},
         "models":[
           {"id":"gpt-6.1-sol","base":{"contextWindow":272000,"maxContextWindow":872000},"overrides":{"maxContextWindow":500000}},
           {"id":"unknown","base":{"contextWindow":32000},"overrides":{"contextWindow":24000,"maxTokens":4096,"remoteCompaction":{"enabled":false,"v2StreamingEnabled":true,"endpoint":"https://old.invalid/model"}}}
         ]}
        """#.utf8))
        var draft = OMPConfiguration(stored: stored)
        draft.set(.model("unknown", "maxTokens", "输出上限"), "8192")
        draft.applyOpenAIPreset()
        XCTAssertEqual(draft.text(.selector("codexpress/*")), "80%")
        XCTAssertEqual(draft.text(.selector("codexpress/unknown")), "")
        XCTAssertEqual(draft.text(.selector("personal/keep")), "75%")
        XCTAssertEqual(draft.text(.model("unknown", "contextWindow", "上下文窗口")), "24000")
        XCTAssertEqual(draft.text(.model("unknown", "maxTokens", "输出上限")), "8192")
        XCTAssertEqual(draft.text(.model("gpt-6.1-sol", "maxContextWindow", "扩展窗口")), "900000")
        XCTAssertEqual(draft.text(.remote("enabled", "远程", model: "unknown", .boolean)), "")
        XCTAssertEqual(draft.text(.remote("endpoint", "端点")), "")
        XCTAssertNotNil(draft.edits(), "继承窗口较小时，推荐值也必须能通过最终容量校验")
        XCTAssertTrue(draft.errors.isEmpty)
    }

    func testSavedImportReplacesSupportedFieldsWithoutCopyingCatalogOrOtherProviders() throws {
        let remote = try JSONDecoder().decode(JSONValue.self, from: Data(#"""
        {"compaction":{"enabled":true,"thresholdTokens":12000,"keepRecentTokens":100,"modelThresholds":{"codexpress/*":"f10000","codexpress/old":"f20000","personal/keep":"75%"}},
         "providerRemoteCompaction":{"enabled":true,"endpoint":"https://remote.invalid/compact","v2StreamingEnabled":true},
         "models":[
           {"id":"shared","base":{"contextWindow":100,"maxContextWindow":300},"overrides":{"contextWindow":200,"maxContextWindow":250,"maxTokens":900,"omitMaxOutputTokens":true,"compactionModel":"old","remoteCompaction":{"enabled":true,"endpoint":"https://remote.invalid/model"},"customTools":["remote-only"]}},
           {"id":"remote-only","base":{"contextWindow":500},"overrides":{"maxTokens":100}}
         ]}
        """#.utf8))
        let local = try JSONDecoder().decode(JSONValue.self, from: Data(#"""
        {"compaction":{"enabled":false,"thresholdPercent":33,"methodOrder":["remote","handoff"],"modelThresholds":{"codexpress/*":null,"codexpress/shared":"f400","personal/keep":"20%"}},
         "providerRemoteCompaction":{"enabled":false,"api":"openai-responses"},
         "models":[
           {"id":"shared","base":{"contextWindow":1000,"maxContextWindow":2000},"overrides":{"maxTokens":128,"omitMaxOutputTokens":false,"remoteCompaction":{"v2StreamingEnabled":false},"apiKey":"never-copy","customTools":["never-copy"]}},
           {"id":"local-only","base":{"contextWindow":8000},"overrides":{"maxTokens":99}}
         ]}
        """#.utf8))
        var draft = OMPConfiguration(stored: remote)
        draft.set(.config("asyncEnabled", "异步压缩", .boolean), "true")
        draft.set(.model("shared", "compactionModel", "压缩模型", .text), "unsaved")
        try draft.addSelector("codexpress/unsaved")
        draft.set(.selector("codexpress/unsaved"), "f123")
        draft.set(.selector("personal/keep"), "66%")

        XCTAssertEqual(draft.importSaved(local), ["local-only"])
        let edits = try XCTUnwrap(draft.edits())
        func edit(_ field: OMPConfiguration.Field) -> JSONValue? {
            edits.first { $0["file"].string == field.file && $0["path"].array.compactMap(\.string) == field.path }
        }
        let removedFields: [OMPConfiguration.Field] = [
            .config("thresholdTokens", "固定阈值", .integer),
            .config("keepRecentTokens", "保留近期预算", .number),
            .remote("endpoint", "端点"),
            .remote("v2StreamingEnabled", "流式 V2", .boolean),
            .model("shared", "contextWindow", "上下文窗口"),
            .model("shared", "maxContextWindow", "扩展窗口"),
            .model("shared", "compactionModel", "压缩模型", .text),
            .remote("enabled", "远程", model: "shared", .boolean),
            .remote("endpoint", "端点", model: "shared"),
            .selector("codexpress/old")
        ]
        for field in removedFields {
            XCTAssertEqual(edit(field)?["remove"], .bool(true), field.path.joined(separator: "."))
        }
        XCTAssertEqual(edit(.config("enabled", "自动压缩", .boolean))?["value"], .bool(false))
        XCTAssertEqual(edit(.config("thresholdPercent", "百分比", .number))?["value"], .number(33))
        XCTAssertEqual(edit(.config("methodOrder", "方法", .methods))?["value"], .array([.string("remote"), .string("handoff")]))
        XCTAssertEqual(edit(.remote("enabled", "远程", .boolean))?["value"], .bool(false))
        XCTAssertEqual(edit(.model("shared", "maxTokens", "输出上限"))?["value"], .number(128))
        XCTAssertEqual(edit(.model("shared", "omitMaxOutputTokens", "省略输出上限参数", .boolean))?["value"], .bool(false))
        XCTAssertEqual(edit(.remote("v2StreamingEnabled", "流式 V2", model: "shared", .boolean))?["value"], .bool(false))
        XCTAssertEqual(edit(.selector("codexpress/*"))?["value"], .null)
        XCTAssertNil(edit(.selector("codexpress/*"))?["remove"].bool)
        XCTAssertEqual(edit(.selector("codexpress/shared"))?["value"], .string("f400"))
        XCTAssertEqual(edit(.selector("personal/keep"))?["value"], .string("66%"))
        XCTAssertNil(edit(.selector("codexpress/unsaved")))
        XCTAssertNil(edit(.config("asyncEnabled", "异步压缩", .boolean)))
        XCTAssertEqual(draft.base("shared", "contextWindow"), .number(100))
        XCTAssertEqual(draft.text(.model("remote-only", "maxTokens", "输出上限")), "100")
        XCTAssertFalse(edits.contains { $0["path"].array.compactMap(\.string).contains(where: { ["apiKey", "customTools", "base", "local-only"].contains($0) }) })
    }

    func testSavedImportValidatesCapacityAgainstRemoteInheritedBase() throws {
        let remote = try JSONDecoder().decode(JSONValue.self, from: Data(#"""
        {"compaction":{},"models":[{"id":"shared","base":{"contextWindow":100,"maxContextWindow":300},"overrides":{"maxContextWindow":1000}}]}
        """#.utf8))
        let local = try JSONDecoder().decode(JSONValue.self, from: Data(#"""
        {"compaction":{},"models":[{"id":"shared","base":{"contextWindow":100,"maxContextWindow":1000},"overrides":{"contextWindow":500}}]}
        """#.utf8))
        var draft = OMPConfiguration(stored: remote)
        XCTAssertTrue(draft.importSaved(local).isEmpty)
        XCTAssertNil(draft.edits(), "移除扩展窗口覆盖后必须按远端基础上限校验，而非复制本地基础上限")
        XCTAssertNotNil(draft.errors[.model("shared", "maxContextWindow", "扩展窗口")])
    }
}
