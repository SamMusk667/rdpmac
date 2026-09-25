// Builds the app's icons from the designs in app/Icons:
//
//   swift scripts/icons.swift ICONS_DIR OUT_DIR
//
// writes OUT_DIR/AppIcon.iconset, the light app icon in the macOS icon shape at every size
// iconutil wants, and a vector PDF of every menu-bar template in ICONS_DIR/menu. The app loads
// the PDFs because AppKit reads PDF on every macOS it supports, SVG not.

import AppKit
import QuartzCore

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("icons: \(message)\n".utf8))
    exit(1)
}

let arguments = CommandLine.arguments
guard arguments.count == 3 else { fail("usage: icons.swift ICONS_DIR OUT_DIR") }
let icons = URL(fileURLWithPath: arguments[1])
let out = URL(fileURLWithPath: arguments[2])
let files = FileManager.default
do { try files.createDirectory(at: out, withIntermediateDirectories: true) } catch { fail("\(error)") }

// MARK: Menu-bar templates

func writePDF(of image: NSImage, to url: URL) {
    var box = CGRect(origin: .zero, size: image.size)
    guard let pdf = CGContext(url as CFURL, mediaBox: &box, nil) else { fail("cannot write \(url.path)") }
    pdf.beginPDFPage(nil)
    NSGraphicsContext.current = NSGraphicsContext(cgContext: pdf, flipped: false)
    image.draw(in: box)
    NSGraphicsContext.current = nil
    pdf.endPDFPage()
    pdf.closePDF()
}

let menu = icons.appendingPathComponent("menu")
let templates = (try? files.contentsOfDirectory(at: menu, includingPropertiesForKeys: nil))?
    .filter { $0.pathExtension == "svg" } ?? []
guard !templates.isEmpty else { fail("no SVG in \(menu.path)") }
for svg in templates {
    guard let image = NSImage(contentsOf: svg), image.size.width > 0 else { fail("cannot read \(svg.path)") }
    writePDF(of: image, to: out.appendingPathComponent(svg.deletingPathExtension().lastPathComponent + ".pdf"))
}

// MARK: App icon

/// The macOS icon grid: on a 1024 canvas the rounded square is 824 wide with a 185.4 corner
/// radius, continuous corners, and a soft shadow below it.
let body: CGFloat = 824 / 1024
let corner: CGFloat = 185.4 / 1024
let master = icons.appendingPathComponent("app/app-light.png")
guard let source = NSImage(contentsOf: master)?.cgImage(forProposedRect: nil, context: nil, hints: nil)
else { fail("cannot read \(master.path)") }

func bitmap(_ side: Int) -> CGContext {
    guard let context = CGContext(
        data: nil, width: side, height: side, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { fail("no bitmap context") }
    context.interpolationQuality = .high
    return context
}

func icon(_ side: Int) -> CGImage {
    let size = CGFloat(side)
    let width = (size * body).rounded()
    // The artwork clipped to the rounded square, drawn by Core Animation for its continuous corners.
    let shape = CALayer()
    shape.frame = CGRect(x: 0, y: 0, width: width, height: width)
    shape.contents = source
    shape.contentsGravity = .resizeAspectFill
    shape.cornerRadius = size * corner
    shape.cornerCurve = .continuous
    shape.masksToBounds = true
    let clipped = bitmap(Int(width))
    shape.render(in: clipped)
    guard let artwork = clipped.makeImage() else { fail("rendering failed") }
    let canvas = bitmap(side)
    canvas.setShadow(offset: CGSize(width: 0, height: -size * 10 / 1024), blur: size * 20 / 1024,
                     color: NSColor.black.withAlphaComponent(0.3).cgColor)
    let inset = ((size - width) / 2).rounded()
    canvas.draw(artwork, in: CGRect(x: inset, y: inset, width: width, height: width))
    guard let image = canvas.makeImage() else { fail("rendering failed") }
    return image
}

let iconset = out.appendingPathComponent("AppIcon.iconset")
try? files.removeItem(at: iconset)
do { try files.createDirectory(at: iconset, withIntermediateDirectories: true) } catch { fail("\(error)") }
for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        let rep = NSBitmapImageRep(cgImage: icon(points * scale))
        guard let png = rep.representation(using: .png, properties: [:]) else { fail("PNG encoding failed") }
        do { try png.write(to: iconset.appendingPathComponent(name)) } catch { fail("\(error)") }
    }
}
