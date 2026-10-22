import XCTest
@testable import Codexpress

final class SourceCapacityTests: XCTestCase {
    private func source(official: Bool = false) -> Source {
        var source = official ? Source.newOfficial() : Source()
        source.name = "测试来源"
        if official { source.proxy.url = "http://127.0.0.1:19876" }
        return source
    }

    func testLegacySourcesRemainUnlimitedWithShortDefaultWindow() throws {
        let legacy = Data(#"{"id":"legacy","name":"旧来源","kind":"api","base_url":"https://api.example.com/v1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"models":[]}"#.utf8)
        let restored = try JSONDecoder().decode(Source.self, from: legacy)
        XCTAssertNil(restored.max_active_conversations)
        XCTAssertNil(restored.active_conversation_window_seconds)
        XCTAssertEqual(restored.activeConversationWindowSeconds, 15)
        XCTAssertNoThrow(try restored.validate())
        XCTAssertEqual(try restored.withConversationCapacity(enabled: false, limit: "", windowSeconds: ""), restored)
        for source in [Source(), Source.newOfficial()] {
            XCTAssertNil(source.max_active_conversations)
            XCTAssertEqual(source.activeConversationWindowSeconds, 15)
        }
    }

    func testEditorInputAndSourceValidationUseSameBoundsForBothKinds() throws {
        for official in [false, true] {
            let original = source(official: official)
            for (limit, window) in [("1", "1"), ("3", "15"), ("1024", "300"), (" 3 ", " 15 ")] {
                let edited = try original.withConversationCapacity(enabled: true, limit: limit, windowSeconds: window)
                XCTAssertNoThrow(try edited.validate())
            }
            for limit in ["", "0", "1025", "-1", "1.5", "+3", "３", "NaN", "99999999999999999999999999"] {
                XCTAssertThrowsError(try original.withConversationCapacity(enabled: true, limit: limit, windowSeconds: "15"), limit)
            }
            for window in ["", "0", "301", "-1", "1.5", "+15", "１５", "NaN", "99999999999999999999999999"] {
                XCTAssertThrowsError(try original.withConversationCapacity(enabled: true, limit: "3", windowSeconds: window), window)
            }
            for limit in [0, 1025] {
                var invalid = original; invalid.max_active_conversations = limit
                XCTAssertThrowsError(try invalid.validate())
            }
            for window in [0, 301] {
                var invalid = original; invalid.active_conversation_window_seconds = window
                XCTAssertThrowsError(try invalid.validate())
            }
        }
    }

    func testSavingCustomCapacityAndDisablingPreserveMetadataAndSyncFields() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-source-capacity-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = MetadataStore(directory: directory)
        let configured = try source().withConversationCapacity(enabled: true, limit: "3", windowSeconds: "12")
        let official = try source(official: true).withConversationCapacity(enabled: true, limit: "7", windowSeconds: "20")
        var settings = AppSettings(); settings.sources = [configured, official]
        try store.save(settings)
        let loaded = try store.load()
        XCTAssertEqual(loaded.sources, [configured, official])
        // AppModel.sync uses this same Codable conversion before adding credentials.
        let syncSource = try JSONValue.from(loaded.sources[0])
        XCTAssertEqual(syncSource["max_active_conversations"].number, 3)
        XCTAssertEqual(syncSource["active_conversation_window_seconds"].number, 12)
        let disabled = try loaded.sources[0].withConversationCapacity(enabled: false, limit: "invalid hidden input", windowSeconds: "invalid hidden input")
        XCTAssertNil(disabled.max_active_conversations)
        XCTAssertEqual(disabled.activeConversationWindowSeconds, 12)
        XCTAssertNoThrow(try disabled.validate())
        settings.sources[0] = disabled
        try store.save(settings)
        XCTAssertEqual(try store.load().sources[0], disabled)
        XCTAssertEqual(try JSONValue.from(disabled)["max_active_conversations"], .null)
        XCTAssertEqual(try JSONValue.from(disabled)["active_conversation_window_seconds"].number, 12)
    }
}
