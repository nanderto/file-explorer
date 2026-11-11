// Draws the app icon and writes a 1024px PNG. `scripts/make_icon.sh` turns
// that into AppIcon.icns. Code rather than a binary asset so the icon is
// diffable, themeable from the same palette the app uses, and regenerable.
import AppKit
import Foundation

let size = 1024.0
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
guard let ctx = NSGraphicsContext.current?.cgContext else { exit(1) }

// Background: the app's dark surface, as a macOS-style rounded square.
let inset = size * 0.06
let rect = CGRect(x: inset, y: inset, width: size - inset * 2, height: size - inset * 2)
let bg = NSBezierPath(roundedRect: rect, xRadius: size * 0.22, yRadius: size * 0.22)
NSColor(calibratedHue: 222/360, saturation: 0.12, brightness: 0.22, alpha: 1).setFill()
bg.fill()

// Folder, in the same blue the file list paints folders with.
let blue = NSColor(calibratedHue: 210/360, saturation: 0.70, brightness: 0.70, alpha: 1)
let w = size * 0.52, h = size * 0.40
let x = (size - w) / 2, y = (size - h) / 2 - size * 0.02
let tabW = w * 0.40, tabH = h * 0.16

let folder = NSBezierPath()
folder.move(to: CGPoint(x: x, y: y))
folder.line(to: CGPoint(x: x + w, y: y))
folder.line(to: CGPoint(x: x + w, y: y + h))
folder.line(to: CGPoint(x: x + tabW + tabH, y: y + h))
folder.line(to: CGPoint(x: x + tabW, y: y + h + tabH))
folder.line(to: CGPoint(x: x, y: y + h + tabH))
folder.close()
blue.setFill()
folder.fill()

// A lighter lip, so the shape reads at 16px rather than becoming a blob.
NSColor(calibratedHue: 210/360, saturation: 0.55, brightness: 0.86, alpha: 1).setFill()
NSBezierPath(
    roundedRect: CGRect(x: x, y: y, width: w, height: h * 0.70),
    xRadius: size * 0.012, yRadius: size * 0.012
).fill()

img.unlockFocus()

guard let tiff = img.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
