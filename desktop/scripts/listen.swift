// 旁听验证：监听系统全局事件流，打印修饰键 / 按键事件。
// 用法：swift listen.swift <秒数>
import AppKit
import Foundation

let duration = Double(CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "8")!
var seen = 0

let mask: NSEvent.EventTypeMask = [.flagsChanged, .keyDown, .keyUp]
_ = NSEvent.addGlobalMonitorForEvents(matching: mask) { e in
    let alt = e.modifierFlags.contains(.option)
    print("[listener] \(e.type) keycode=\(e.keyCode) alt=\(alt) rawFlags=0x\(String(e.modifierFlags.rawValue, radix: 16))")
    fflush(stdout)
    seen += 1
}

print("[listener] listening for \(duration)s …")
fflush(stdout)
RunLoop.main.run(until: Date().addingTimeInterval(duration))
print("[listener] done, events seen: \(seen)")
exit(seen > 0 ? 0 : 1)
