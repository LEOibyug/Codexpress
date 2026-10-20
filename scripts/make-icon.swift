import AppKit
import Foundation

let destination = CommandLine.arguments[1]
let iconset = URL(fileURLWithPath: destination, isDirectory: true)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
func render(_ size: Int) -> Data {
    let image = NSImage(size: NSSize(width: size, height: size))
    image.lockFocus()
    let scale = CGFloat(size) / 1024
    let t = NSAffineTransform()
    t.scaleX(by: scale, yBy: scale)
    t.concat()
    let rect = NSRect(x: 55, y: 55, width: 914, height: 914)
    let background = NSBezierPath(roundedRect: rect, xRadius: 208, yRadius: 208)
    NSGradient(starting: NSColor(calibratedRed: 0.13, green: 0.22, blue: 0.30, alpha: 1), ending: NSColor(calibratedRed: 0.045, green: 0.09, blue: 0.15, alpha: 1))!.draw(in: background, angle: -55)
    let path = NSBezierPath()
    path.move(to: NSPoint(x: 360, y: 293)); path.line(to: NSPoint(x: 360, y: 710))
    path.move(to: NSPoint(x: 360, y: 425))
    path.curve(to: NSPoint(x: 661, y: 670), controlPoint1: NSPoint(x: 360, y: 603), controlPoint2: NSPoint(x: 661, y: 490))
    path.lineWidth = 48; path.lineCapStyle = .round
    NSColor(calibratedRed: 0.69, green: 0.86, blue: 0.99, alpha: 1).setStroke(); path.stroke()
    for point in [NSPoint(x: 360, y: 293), NSPoint(x: 360, y: 710), NSPoint(x: 661, y: 670)] {
        NSColor(calibratedRed: 0.93, green: 0.97, blue: 1, alpha: 1).setFill()
        NSBezierPath(ovalIn: NSRect(x: point.x - 66, y: point.y - 66, width: 132, height: 132)).fill()
        NSColor(calibratedRed: 0.15, green: 0.32, blue: 0.45, alpha: 1).setFill()
        NSBezierPath(ovalIn: NSRect(x: point.x - 27, y: point.y - 27, width: 54, height: 54)).fill()
    }
    image.unlockFocus()
    let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
    return rep.representation(using: .png, properties: [:])!
}
for size in [16, 32, 128, 256, 512] {
    try render(size).write(to: iconset.appendingPathComponent("icon_\(size)x\(size).png"))
    try render(size * 2).write(to: iconset.appendingPathComponent("icon_\(size)x\(size)@2x.png"))
}
