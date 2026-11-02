import XCTest
@testable import Codexpress

final class OMPInstallationTests: XCTestCase {
    func testFindsExecutableOnPATHWithoutShellInterpolationOrConfigCommand() async throws {
        let home = try temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let bin = home.appendingPathComponent("bin with spaces;$literal")
        let executable = try fakeOMP(in: bin, body: #"[ "$#" = 1 ] && [ "$1" = --version ] || exit 91; printf 'omp/18.15.0\n'"#)
        let result = await OMPInstallation.detect(environment: ["PATH": bin.path], home: home)
        XCTAssertEqual(result.executable, executable.path)
        XCTAssertEqual(result.version, "18.15.0")
        XCTAssertEqual(result.agentDirectory, home.appendingPathComponent(".omp/agent").path)
        XCTAssertFalse(FileManager.default.fileExists(atPath: home.appendingPathComponent(".omp").path))
    }

    func testResolvesOverridesProfilesAndXDGWithoutReadingOrCreatingState() async throws {
        let home = try temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let override = home.appendingPathComponent("custom state")
        let configured = await OMPInstallation.detect(environment: ["PI_CODING_AGENT_DIR": override.path], home: home, candidatePaths: [])
        XCTAssertEqual(configured.agentDirectory, override.path)
        XCTAssertNil(configured.executable)
        let profile = await OMPInstallation.detect(environment: ["PI_CONFIG_DIR": ".other-omp", "OMP_PROFILE": "work", "PI_CODING_AGENT_DIR": override.path, "XDG_DATA_HOME": home.appendingPathComponent("data").path], home: home, candidatePaths: [])
        XCTAssertEqual(profile.agentDirectory, home.appendingPathComponent(".other-omp/profiles/work/agent").path)
        let legacyProfile = await OMPInstallation.detect(environment: ["PI_PROFILE": "legacy"], home: home, candidatePaths: [])
        XCTAssertEqual(legacyProfile.agentDirectory, home.appendingPathComponent(".omp/profiles/legacy/agent").path)
        let defaultProfile = await OMPInstallation.detect(environment: ["OMP_PROFILE": "", "PI_PROFILE": "legacy", "PI_CODING_AGENT_DIR": "relative/agent"], home: home, candidatePaths: [], currentDirectory: home)
        XCTAssertEqual(defaultProfile.agentDirectory, home.appendingPathComponent("relative/agent").path)
        let invalid = await OMPInstallation.detect(environment: ["OMP_PROFILE": "../../outside"], home: home, candidatePaths: [])
        XCTAssertEqual(invalid.agentDirectory, home.appendingPathComponent(".omp/agent").path)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: home.path), [])
    }

    func testRejectsUntrustedOutputAndBoundsOutputAndRuntime() async throws {
        let home = try temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let invalid = try fakeOMP(in: home.appendingPathComponent("invalid"), body: "printf 'private unexpected failure\\n'; exit 1")
        let invalidResult = await OMPInstallation.detect(environment: [:], home: home, candidatePaths: [invalid.path])
        XCTAssertEqual(invalidResult.executable, invalid.path)
        XCTAssertNil(invalidResult.version)
        let oversized = try fakeOMP(in: home.appendingPathComponent("oversized"), body: "printf '%09000d' 0")
        let oversizedResult = await OMPInstallation.detect(environment: [:], home: home, candidatePaths: [oversized.path])
        XCTAssertNil(oversizedResult.version)
        let hanging = try fakeOMP(in: home.appendingPathComponent("hanging"), body: "trap '' TERM; while :; do :; done")
        let start = ProcessInfo.processInfo.systemUptime
        let hangingResult = await OMPInstallation.detect(environment: [:], home: home, candidatePaths: [hanging.path], timeout: 0.1)
        XCTAssertLessThan(ProcessInfo.processInfo.systemUptime - start, 2)
        XCTAssertEqual(hangingResult.executable, hanging.path)
        XCTAssertNil(hangingResult.version)
    }

    private func temporaryHome() throws -> URL {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        return home
    }

    private func fakeOMP(in directory: URL, body: String) throws -> URL {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let executable = directory.appendingPathComponent("omp")
        try ("#!/bin/sh\n" + body + "\n").write(to: executable, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: executable.path)
        return executable
    }
}
