// swift-tools-version: 5.9
import PackageDescription
let package = Package(name: "Codexpress", platforms: [.macOS(.v14)], products: [.executable(name: "Codexpress", targets: ["Codexpress"])], targets: [.executableTarget(name: "Codexpress"), .testTarget(name: "CodexpressTests", dependencies: ["Codexpress"])])
