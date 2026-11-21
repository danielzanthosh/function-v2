import AppKit
import CoreGraphics
import CoreImage
import CoreMedia
import Darwin
import Foundation
import ScreenCaptureKit

@MainActor
final class AgentCursorView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        let pointer = NSBezierPath()
        pointer.move(to: NSPoint(x: 2, y: 31))
        pointer.line(to: NSPoint(x: 2, y: 6))
        pointer.line(to: NSPoint(x: 9, y: 13))
        pointer.line(to: NSPoint(x: 13, y: 3))
        pointer.line(to: NSPoint(x: 17, y: 5))
        pointer.line(to: NSPoint(x: 13, y: 15))
        pointer.line(to: NSPoint(x: 22, y: 15))
        pointer.close()
        NSColor.white.setFill()
        pointer.fill()
        NSColor(calibratedWhite: 0.08, alpha: 1).setStroke()
        pointer.lineWidth = 2
        pointer.stroke()
    }
}

@MainActor
final class AgentCursorPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

@MainActor
final class AgentCursorOverlay {
    static let shared = AgentCursorOverlay()
    private let size = NSSize(width: 24, height: 32)
    private var panel: AgentCursorPanel?
    private var targetWindow: SCWindow?
    private var point: CGPoint?
    private var hideWork: DispatchWorkItem?
    private var activationObserver: NSObjectProtocol?

    private init() {
        activationObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didActivateApplicationNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
    }

    func show(in window: SCWindow, at point: CGPoint) {
        targetWindow = window
        self.point = point
        hideWork?.cancel()
        let hideWork = DispatchWorkItem { [weak self] in self?.hide() }
        self.hideWork = hideWork
        DispatchQueue.main.asyncAfter(deadline: .now() + .seconds(2), execute: hideWork)
        refresh()
    }

    func hide() {
        hideWork?.cancel()
        hideWork = nil
        targetWindow = nil
        point = nil
        panel?.orderOut(nil)
    }

    private func refresh() {
        guard let targetWindow,
              let point,
              targetWindow.isOnScreen,
              let processID = targetWindow.owningApplication?.processID,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == processID,
              let layer = windowLayer(targetWindow.windowID),
              layer <= Int(CGWindowLevelForKey(.floatingWindow)) else {
            panel?.orderOut(nil)
            return
        }

        let panel = self.panel ?? makePanel()
        self.panel = panel
        panel.level = NSWindow.Level(rawValue: layer)
        let mainDisplayHeight = CGDisplayBounds(CGMainDisplayID()).height
        panel.setFrameOrigin(NSPoint(x: point.x, y: mainDisplayHeight - point.y - size.height))
        panel.order(.above, relativeTo: Int(targetWindow.windowID))
    }

    private func makePanel() -> AgentCursorPanel {
        let panel = AgentCursorPanel(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.backgroundColor = .clear
        panel.isOpaque = false
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
        panel.contentView = AgentCursorView(frame: NSRect(origin: .zero, size: size))
        return panel
    }

    private func windowLayer(_ windowID: CGWindowID) -> Int? {
        let windows = CGWindowListCopyWindowInfo(.optionIncludingWindow, windowID) as? [[String: Any]]
        return windows?.first?[kCGWindowLayer as String] as? Int
    }
}

final class FrameOutput: NSObject, SCStreamOutput {
    private let lock = NSLock()
    private let context = CIContext()
    private var frame: Data?
    private var size = CGSize.zero

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen,
              let buffer = CMSampleBufferGetImageBuffer(sampleBuffer),
              let data = context.pngRepresentation(of: CIImage(cvPixelBuffer: buffer), format: .RGBA8, colorSpace: CGColorSpaceCreateDeviceRGB()) else { return }
        lock.lock()
        frame = data
        size = CGSize(width: CVPixelBufferGetWidth(buffer), height: CVPixelBufferGetHeight(buffer))
        lock.unlock()
    }

    func snapshot() -> (Data, CGSize)? {
        lock.lock()
        defer { lock.unlock() }
        guard let frame else { return nil }
        return (frame, size)
    }
}

final class StreamRegistry {
    static let shared = StreamRegistry()
    private let lock = NSLock()
    private var streams: [UInt32: (SCStream, FrameOutput)] = [:]

    func get(_ windowID: UInt32) -> (SCStream, FrameOutput)? {
        lock.lock(); defer { lock.unlock() }
        return streams[windowID]
    }

    func set(_ windowID: UInt32, _ stream: SCStream, _ output: FrameOutput) {
        lock.lock(); streams[windowID] = (stream, output); lock.unlock()
    }

    func remove(_ windowID: UInt32) -> (SCStream, FrameOutput)? {
        lock.lock(); defer { lock.unlock() }
        return streams.removeValue(forKey: windowID)
    }

    func stopAll() async {
        let active = lock.withLock {
            let active = Array(streams.values)
            streams.removeAll()
            return active
        }
        for (stream, _) in active { try? await stream.stopCapture() }
    }
}

let outputLock = NSLock()

func writeMessage(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object) else { return }
    outputLock.lock()
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([10]))
    outputLock.unlock()
}

func getContent() async throws -> SCShareableContent {
    try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
}

func findWindow(_ id: UInt32) async throws -> SCWindow {
    guard let window = try await getContent().windows.first(where: { $0.windowID == id }) else {
        throw NSError(domain: "FunctionNativeHelper", code: -32004, userInfo: [NSLocalizedDescriptionKey: "Target window is no longer available"])
    }
    return window
}

func startStream(_ id: UInt32) async throws {
    if StreamRegistry.shared.get(id) != nil { return }
    let window = try await findWindow(id)
    let filter = SCContentFilter(desktopIndependentWindow: window)
    let configuration = SCStreamConfiguration()
    configuration.showsCursor = false
    configuration.capturesAudio = false
    configuration.width = max(1, Int(window.frame.width * 2))
    configuration.height = max(1, Int(window.frame.height * 2))
    configuration.minimumFrameInterval = CMTime(value: 1, timescale: 5)
    configuration.queueDepth = 2
    let stream = SCStream(filter: filter, configuration: configuration, delegate: nil)
    let output = FrameOutput()
    try stream.addStreamOutput(output, type: .screen, sampleHandlerQueue: DispatchQueue(label: "function.native-helper.frames"))
    try await stream.startCapture()
    StreamRegistry.shared.set(id, stream, output)
}

func accessibilityPermissionRequired() -> NSError? {
    AXIsProcessTrusted() ? nil : NSError(domain: "FunctionNativeHelper", code: -32002, userInfo: [NSLocalizedDescriptionKey: "Accessibility permission is required. Enable Function in System Settings → Privacy & Security → Accessibility."])
}

func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
    return value
}

func accessibilitySnapshot(_ window: SCWindow) throws -> [String: Any] {
    if let error = accessibilityPermissionRequired() { throw error }
    guard let pid = window.owningApplication?.processID else {
        throw NSError(domain: "FunctionNativeHelper", code: -32004, userInfo: [NSLocalizedDescriptionKey: "Target app process is unavailable"])
    }
    let application = AXUIElementCreateApplication(pid)
    guard let windowValue = attribute(application, kAXWindowsAttribute), let windows = windowValue as? [AXUIElement] else {
        throw NSError(domain: "FunctionNativeHelper", code: -32007, userInfo: [NSLocalizedDescriptionKey: "Target app does not expose accessible windows"])
    }
    var elements = [[String: Any]]()
    func visit(_ element: AXUIElement, _ ref: String, _ depth: Int) {
        guard elements.count < 400, depth < 14 else { return }
        let role = (attribute(element, kAXRoleAttribute) as? String) ?? "Unknown"
        let label = (attribute(element, kAXTitleAttribute) as? String)
            ?? (attribute(element, kAXDescriptionAttribute) as? String)
            ?? (attribute(element, kAXValueAttribute) as? String)
            ?? ""
        var actionValue: CFArray?
        _ = AXUIElementCopyActionNames(element, &actionValue)
        let actions = (actionValue as? [String]) ?? []
        elements.append(["ref": ref, "role": role, "label": label, "actions": actions])
        if let childValue = attribute(element, kAXChildrenAttribute), let children = childValue as? [AXUIElement] {
            for (index, child) in children.enumerated() {
                visit(child, "\(ref)/\(index)", depth + 1)
                if elements.count >= 400 { return }
            }
        }
    }
    for (index, item) in windows.enumerated() {
        visit(item, "w\(index)", 0)
        if elements.count >= 400 { break }
    }
    return ["windowId": window.windowID, "elements": elements]
}

func performAccessibilityAction(_ window: SCWindow, _ params: [String: Any]) throws -> [String: Any] {
    if let error = accessibilityPermissionRequired() { throw error }
    guard let pid = window.owningApplication?.processID,
          let ref = params["ref"] as? String,
          let operation = params["operation"] as? String else {
        throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Expected ref and operation"])
    }
    let parts = ref.split(separator: "/").map(String.init)
    guard let first = parts.first, first.first == "w", let windowIndex = Int(first.dropFirst()) else {
        throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Invalid accessibility ref"])
    }
    let application = AXUIElementCreateApplication(pid)
    guard let value = attribute(application, kAXWindowsAttribute), let windows = value as? [AXUIElement], windows.indices.contains(windowIndex) else {
        throw NSError(domain: "FunctionNativeHelper", code: -32004, userInfo: [NSLocalizedDescriptionKey: "Accessibility window changed; take a new snapshot"])
    }
    var element = windows[windowIndex]
    for part in parts.dropFirst() {
        guard let index = Int(part), let childValue = attribute(element, kAXChildrenAttribute),
              let children = childValue as? [AXUIElement], children.indices.contains(index) else {
            throw NSError(domain: "FunctionNativeHelper", code: -32004, userInfo: [NSLocalizedDescriptionKey: "Accessibility element changed; take a new snapshot"])
        }
        element = children[index]
    }
    let result: AXError
    switch operation {
    case "press": result = AXUIElementPerformAction(element, kAXPressAction as CFString)
    case "focus": result = AXUIElementSetAttributeValue(element, kAXFocusedAttribute as CFString, kCFBooleanTrue)
    case "set_value":
        guard let text = params["value"] as? String else { throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "set_value requires a string value"]) }
        result = AXUIElementSetAttributeValue(element, kAXValueAttribute as CFString, text as CFString)
    default: throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Unsupported accessibility operation"])
    }
    guard result == .success else {
        throw NSError(domain: "FunctionNativeHelper", code: Int(result.rawValue), userInfo: [NSLocalizedDescriptionKey: "Accessibility action failed with code \(result.rawValue)"])
    }
    return ["sent": true, "windowId": window.windowID, "ref": ref, "operation": operation]
}

@MainActor
func handle(_ method: String, _ params: [String: Any]) async throws -> Any {
    switch method {
    case "listWindows":
        let content = try await getContent()
        return content.windows.filter { $0.isOnScreen && $0.frame.width > 0 && $0.frame.height > 0 }.map { window in
            [
                "id": window.windowID,
                "process_id": window.owningApplication?.processID ?? 0,
                "title": window.title ?? window.owningApplication?.applicationName ?? "Untitled window",
                "x": window.frame.origin.x, "y": window.frame.origin.y,
                "width": window.frame.width, "height": window.frame.height,
            ] as [String: Any]
        }
    case "startStream":
        try await startStream(try uintParam("windowId", params))
        return ["started": true]
    case "captureWindow":
        let id = try uintParam("windowId", params)
        try await startStream(id)
        for _ in 0..<100 {
            if let (data, size) = StreamRegistry.shared.get(id)?.1.snapshot() {
                return ["pngBase64": data.base64EncodedString(), "width": Int(size.width), "height": Int(size.height)]
            }
            try await Task.sleep(for: .milliseconds(20))
        }
        throw NSError(domain: "FunctionNativeHelper", code: -32005, userInfo: [NSLocalizedDescriptionKey: "Timed out waiting for the first window frame"])
    case "stopStream":
        let id = try uintParam("windowId", params)
        if let (stream, _) = StreamRegistry.shared.remove(id) { try await stream.stopCapture() }
        return ["stopped": true]
    case "accessibilitySnapshot":
        return try accessibilitySnapshot(await findWindow(uintParam("windowId", params)))
    case "accessibilityAction":
        return try performAccessibilityAction(await findWindow(uintParam("windowId", params)), params)
    case "click", "movePointer", "scroll", "drag", "typeText", "pressKey", "pressShortcut":
        if let error = accessibilityPermissionRequired() { throw error }
        let id = try uintParam("windowId", params)
        let window = try await findWindow(id)
        let pid = window.owningApplication?.processID ?? 0
        guard pid > 0 else { throw NSError(domain: "FunctionNativeHelper", code: -32004, userInfo: [NSLocalizedDescriptionKey: "Target app process is unavailable"]) }
        if method == "click" || method == "movePointer" || method == "scroll" {
            guard let x = params["x"] as? Double, let y = params["y"] as? Double else {
                throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Expected numeric x and y coordinates"])
            }
            let button = (params["button"] as? String ?? "left").lowercased()
            let mouseButton: CGMouseButton = button == "right" ? .right : (button == "middle" ? .center : .left)
            let types: (CGEventType, CGEventType) = button == "right" ? (.rightMouseDown, .rightMouseUp) : (button == "middle" ? (.otherMouseDown, .otherMouseUp) : (.leftMouseDown, .leftMouseUp))
            if method == "movePointer" {
                guard let event = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: .left) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create pointer event"])
                }
                event.postToPid(pid)
                AgentCursorOverlay.shared.show(in: window, at: CGPoint(x: x, y: y))
            } else if method == "scroll" {
                let delta = Int32((params["delta"] as? Int) ?? 0)
                guard let event = CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 1, wheel1: delta, wheel2: 0, wheel3: 0) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create scroll event"])
                }
                event.location = CGPoint(x: x, y: y)
                event.postToPid(pid)
                AgentCursorOverlay.shared.show(in: window, at: CGPoint(x: x, y: y))
            } else {
                guard let down = CGEvent(mouseEventSource: nil, mouseType: types.0, mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: mouseButton),
                      let up = CGEvent(mouseEventSource: nil, mouseType: types.1, mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: mouseButton) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create click events"])
                }
                let clickCount = min(2, max(1, (params["clickCount"] as? Int) ?? 1))
                for count in 1...clickCount {
                    down.setIntegerValueField(.mouseEventClickState, value: Int64(count))
                    up.setIntegerValueField(.mouseEventClickState, value: Int64(count))
                    down.postToPid(pid); up.postToPid(pid)
                    if count < clickCount { try await Task.sleep(for: .milliseconds(50)) }
                }
                AgentCursorOverlay.shared.show(in: window, at: CGPoint(x: x, y: y))
            }
            return ["sent": true, "windowId": id, "x": x, "y": y]
        }
        if method == "drag" {
            guard let sx = params["startX"] as? Double, let sy = params["startY"] as? Double,
                  let ex = params["endX"] as? Double, let ey = params["endY"] as? Double,
                  let down = CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: CGPoint(x: sx, y: sy), mouseButton: .left),
                  let up = CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: CGPoint(x: ex, y: ey), mouseButton: .left) else {
                throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Invalid drag coordinates"])
            }
            down.postToPid(pid)
            AgentCursorOverlay.shared.show(in: window, at: CGPoint(x: sx, y: sy))
            for step in 1...12 {
                let progress = Double(step) / 12
                let point = CGPoint(x: sx + (ex - sx) * progress, y: sy + (ey - sy) * progress)
                guard let drag = CGEvent(mouseEventSource: nil, mouseType: .leftMouseDragged, mouseCursorPosition: point, mouseButton: .left) else { continue }
                drag.postToPid(pid)
                AgentCursorOverlay.shared.show(in: window, at: point)
                try await Task.sleep(for: .milliseconds(8))
            }
            up.postToPid(pid)
            return ["sent": true, "windowId": id]
        }
        if method == "typeText" {
            guard let text = params["text"] as? String else { throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Expected text string"]) }
            for character in text {
                let units = Array(String(character).utf16)
                guard let down = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true), let up = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create keyboard events"])
                }
                units.withUnsafeBufferPointer { ptr in
                    down.keyboardSetUnicodeString(stringLength: units.count, unicodeString: ptr.baseAddress!)
                    up.keyboardSetUnicodeString(stringLength: units.count, unicodeString: ptr.baseAddress!)
                }
                down.postToPid(pid); up.postToPid(pid)
            }
            return ["sent": true, "windowId": id, "characters": text.count]
        }
        guard let key = params["key"] as? String, let keyCode = keyCode(key) else {
            if method == "pressShortcut" {
                guard let keys = params["keys"] as? [String], let mainKey = keys.last, let code = keyCode(mainKey) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Unsupported key chord"])
                }
                var flags: CGEventFlags = []
                for key in keys.dropLast().map({ $0.lowercased() }) {
                    switch key {
                    case "cmd", "command": flags.insert(.maskCommand)
                    case "ctrl", "control": flags.insert(.maskControl)
                    case "shift": flags.insert(.maskShift)
                    case "alt", "option": flags.insert(.maskAlternate)
                    default: break
                    }
                }
                guard let down = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: true), let up = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: false) else {
                    throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create shortcut events"])
                }
                down.flags = flags; up.flags = flags
                down.postToPid(pid); up.postToPid(pid)
                return ["sent": true, "windowId": id, "keys": keys]
            }
            throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Unsupported key"])
        }
        guard let down = CGEvent(keyboardEventSource: nil, virtualKey: keyCode, keyDown: true), let up = CGEvent(keyboardEventSource: nil, virtualKey: keyCode, keyDown: false) else {
            throw NSError(domain: "FunctionNativeHelper", code: -32006, userInfo: [NSLocalizedDescriptionKey: "Could not create keyboard events"])
        }
        down.postToPid(pid); up.postToPid(pid)
        return ["sent": true, "windowId": id, "key": key]
    case "shutdown":
        await StreamRegistry.shared.stopAll()
        return ["stopped": true]
    default:
        throw NSError(domain: "FunctionNativeHelper", code: -32601, userInfo: [NSLocalizedDescriptionKey: "Unknown method: \(method)"])
    }
}

func uintParam(_ key: String, _ params: [String: Any]) throws -> UInt32 {
    guard let number = params[key] as? NSNumber, number.int64Value >= 0, number.int64Value <= Int64(UInt32.max) else {
        throw NSError(domain: "FunctionNativeHelper", code: -32602, userInfo: [NSLocalizedDescriptionKey: "Expected valid \(key)"])
    }
    return number.uint32Value
}

func keyCode(_ key: String) -> CGKeyCode? {
    switch key.lowercased() {
    case "return", "enter": 36
    case "tab": 48
    case "space": 49
    case "delete", "backspace": 51
    case "escape", "esc": 53
    case "up": 126
    case "down": 125
    case "left": 123
    case "right": 124
    default: nil
    }
}

@MainActor
func stopHelper(_ application: NSApplication) {
    application.stop(nil)
    if let wake = NSEvent.otherEvent(
        with: .applicationDefined,
        location: .zero,
        modifierFlags: [],
        timestamp: 0,
        windowNumber: 0,
        context: nil,
        subtype: 0,
        data1: 0,
        data2: 0
    ) {
        application.postEvent(wake, atStart: true)
    }
}

@MainActor
func processMessage(_ data: Data, application: NSApplication) {
    guard let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          request["jsonrpc"] as? String == "2.0",
          let id = request["id"], let method = request["method"] as? String else {
        writeMessage(["jsonrpc": "2.0", "id": NSNull(), "error": ["code": -32600, "message": "Invalid JSON-RPC request"]])
        return
    }
    let params = request["params"] as? [String: Any] ?? [:]
    Task { @MainActor in
        do {
            let result = try await handle(method, params)
            writeMessage(["jsonrpc": "2.0", "id": id, "result": result])
        } catch let error as NSError {
            writeMessage(["jsonrpc": "2.0", "id": id, "error": ["code": error.code == 0 ? -32000 : error.code, "message": error.localizedDescription]])
        }
        if method == "shutdown" {
            AgentCursorOverlay.shared.hide()
            stopHelper(application)
        }
    }
}

@MainActor
func runHelper() {
    let application = NSApplication.shared
    application.setActivationPolicy(.accessory)
    let input = DispatchSource.makeReadSource(fileDescriptor: STDIN_FILENO, queue: .main)
    var pending = Data()
    input.setEventHandler {
        var bytes = [UInt8](repeating: 0, count: 8192)
        let count = bytes.withUnsafeMutableBytes { buffer in
            Darwin.read(STDIN_FILENO, buffer.baseAddress, buffer.count)
        }
        guard count > 0 else {
            input.cancel()
            Task { @MainActor in
                await StreamRegistry.shared.stopAll()
                AgentCursorOverlay.shared.hide()
                stopHelper(application)
            }
            return
        }
        pending.append(contentsOf: bytes.prefix(count))
        while let newline = pending.firstIndex(of: 10) {
            let line = Data(pending[..<newline])
            pending.removeSubrange(...newline)
            if !line.isEmpty { processMessage(line, application: application) }
        }
    }
    input.resume()
    application.run()
}

MainActor.assumeIsolated { runHelper() }

