import CoreGraphics
import CoreText
import Foundation
let lines = try! String(contentsOfFile: CommandLine.arguments[1], encoding: .utf8).split(separator: "\n", omittingEmptySubsequences: false)
var box = CGRect(x: 0, y: 0, width: 612, height: 792)
let ctx = CGContext(URL(fileURLWithPath: CommandLine.arguments[2]) as CFURL, mediaBox: &box, nil)!
ctx.beginPDFPage(nil)
let font = CTFontCreateWithName("Helvetica" as CFString, 11, nil)
var y: CGFloat = 740
for line in lines {
    let text = NSAttributedString(string: String(line), attributes: [NSAttributedString.Key(kCTFontAttributeName as String): font])
    ctx.textPosition = CGPoint(x: 50, y: y)
    CTLineDraw(CTLineCreateWithAttributedString(text), ctx)
    y -= 22
}
ctx.endPDFPage()
ctx.closePDF()
