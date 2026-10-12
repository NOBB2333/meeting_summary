// Thin C ABI bridge to Apple frameworks. Recording, mixing, encoding and state
// management stay in Rust; no helper process or virtual audio driver is used.
import AppKit
import ScreenCaptureKit
import AVFoundation
import CoreMedia
import CoreVideo
import Carbon
import CoreText

public typealias Callback = @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafeRawPointer?, Int, Int32, Int32, Int32, Int64) -> Void

func jsonString(_ value: Any) -> UnsafeMutablePointer<CChar>? {
    guard let data = try? JSONSerialization.data(withJSONObject: value),
          let string = String(data: data, encoding: .utf8) else { return nil }
    return strdup(string)
}
func arguments(_ input: UnsafePointer<CChar>) -> [String: Any] {
    let data = Data(String(cString: input).utf8)
    return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
}
struct NativeError: Error, CustomStringConvertible {
    let description: String
    init(_ text: String) { description = text }
}

// ScreenCaptureKit completion handlers run independently of the AppKit thread.
// Bound every wait so unavailable devices never hang shutdown or the GUI.
private let consentLock = NSLock()
private var consentRequested = false
func requestScreenConsent() {
    guard !CGPreflightScreenCaptureAccess() else { return }
    consentLock.lock()
    let request = !consentRequested
    consentRequested = true
    consentLock.unlock()
    if request {
        // A system consent dialog must not keep the first GPUI window from
        // appearing. The actual stream operation still reports a denial.
        DispatchQueue.global(qos: .userInitiated).async { _ = CGRequestScreenCaptureAccess() }
    }
}
func content() throws -> SCShareableContent {
    // Preflight is only a reason to request consent. Use ScreenCaptureKit's
    // actual result; a checked Settings switch may belong to an older signature.
    requestScreenConsent()
    let done = DispatchSemaphore(value: 0)
    var result: SCShareableContent?
    var failure: Error?
    SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { value, error in
        result = value; failure = error; done.signal()
    }
    guard done.wait(timeout: .now() + 10) == .success else { throw NativeError("获取屏幕列表超时") }
    if let failure = failure {
        if (failure as NSError).code == -3801 {
            throw NativeError("屏幕与系统音频权限未生效（ScreenCaptureKit -3801）。请在系统设置中允许 OpenCap 并重新打开；若已开启，移除旧 OpenCap 条目后添加当前 OpenCap.app，开发版重新构建会改变签名")
        }
        throw NativeError("ScreenCaptureKit 无法获取录制目标：\(failure.localizedDescription)（\((failure as NSError).code)）")
    }
    guard let result = result else { throw NativeError("没有可用的屏幕") }
    return result
}
func displays(_ value: SCShareableContent) -> [SCDisplay] {
    value.displays.sorted { a, b in
        if a.displayID == b.displayID { return false }
        if a.displayID == CGMainDisplayID() { return true }
        if b.displayID == CGMainDisplayID() { return false }
        return a.displayID < b.displayID
    }
}
func pixelSize(_ id: CGDirectDisplayID) -> (width: Int, height: Int) {
    if let mode = CGDisplayCopyDisplayMode(id) { return (mode.pixelWidth, mode.pixelHeight) }
    return (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id))
}
func windowScale(_ w: SCWindow, _ c: SCShareableContent) -> Double {
    if #available(macOS 14.0, *) { return Double(SCContentFilter(desktopIndependentWindow: w).pointPixelScale) }
    let source = c.displays.max { a, b in
        let aa = CGDisplayBounds(a.displayID).intersection(w.frame)
        let bb = CGDisplayBounds(b.displayID).intersection(w.frame)
        return (aa.isNull ? 0 : aa.width * aa.height) < (bb.isNull ? 0 : bb.width * bb.height)
    }
    return source.map { Double(pixelSize($0.displayID).width) / CGDisplayBounds($0.displayID).width } ?? 1
}

@_cdecl("oc_free") public func ocFree(_ pointer: UnsafeMutableRawPointer?) { free(pointer) }
@_cdecl("oc_sources") public func ocSources() -> UnsafeMutablePointer<CChar>? {
    do {
        let c = try content()
        let monitors: [[String: Any]] = displays(c).enumerated().map { index, d in
            let bounds = CGDisplayBounds(d.displayID)
            let size = pixelSize(d.displayID)
            return ["id": index, "name": "显示器 \(index + 1)", "width": size.width,
                    "height": size.height, "x": bounds.origin.x, "y": bounds.origin.y,
                    "points_width": bounds.width, "points_height": bounds.height]
        }
        let windows: [[String: Any]] = c.windows.filter {
            $0.windowLayer == 0 && $0.frame.width > 1 && $0.frame.height > 1 && $0.owningApplication?.processID != getpid()
        }.map { w in
            let app = w.owningApplication?.applicationName ?? "窗口"
            let scale = windowScale(w, c)
            return ["id": w.windowID, "name": "\(app) — \(w.title ?? "无标题")",
                    "width": Int(w.frame.width * scale), "height": Int(w.frame.height * scale)]
        }
        return jsonString(["monitors": monitors, "windows": windows])
    } catch { return jsonString(["error": String(describing: error)]) }
}

@_cdecl("oc_devices") public func ocDevices() -> UnsafeMutablePointer<CChar>? {
    let types: [AVCaptureDevice.DeviceType]
    if #available(macOS 14.0, *) { types = [.microphone] } else { types = [.builtInMicrophone, .externalUnknown] }
    let devices = AVCaptureDevice.DiscoverySession(deviceTypes: types, mediaType: .audio, position: .unspecified).devices.map { ["id": $0.uniqueID, "name": $0.localizedName] }
    return jsonString(devices)
}
@_cdecl("oc_microphone_authorized") public func ocMicrophoneAuthorized() -> Bool {
    AVCaptureDevice.authorizationStatus(for: .audio) == .authorized
}

final class StreamCapture: NSObject, SCStreamOutput, SCStreamDelegate {
    let callback: Callback
    let user: UnsafeMutableRawPointer?
    let queue = DispatchQueue(label: "org.opencap.capture", qos: .userInitiated)
    let lock = NSLock()
    var accepting = true
    var stream: SCStream?
    init(_ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?) {
        self.callback = callback; self.user = user
    }
    func report(_ error: Error) {
        lock.lock(); defer { lock.unlock() }
        guard accepting else { return }
        let message = String(describing: error)
        message.withCString { callback(user, -1, $0, message.utf8.count, 0, 0, 0, 0) }
    }
    func start(_ a: [String: Any]) throws {
        let c = try content()
        let ds = displays(c)
        let index = a["monitor"] as? Int ?? 0
        guard ds.indices.contains(index) else { throw NativeError("显示器不存在，请刷新录制目标") }
        let d = ds[index]
        let config = SCStreamConfiguration()
        let audioOnly = a["audio_only"] as? Bool ?? false
        let filter: SCContentFilter
        if let id = a["window"] as? UInt32 {
            guard let w = c.windows.first(where: { $0.windowID == id }) else { throw NativeError("窗口已关闭，请重新选择") }
            filter = SCContentFilter(desktopIndependentWindow: w)
            let scale = windowScale(w, c)
            config.width = max(2, Int(w.frame.width * scale))
            config.height = max(2, Int(w.frame.height * scale))
        } else {
            let excluded = (a["exclude_app"] as? Bool ?? true) ? c.applications.filter { $0.processID == getpid() } : []
            filter = SCContentFilter(display: d, excludingApplications: excluded, exceptingWindows: [])
            let size = pixelSize(d.displayID)
            config.width = size.width
            config.height = size.height
            if let r = a["region"] as? [Int], r.count == 4 {
                let sx = CGDisplayBounds(d.displayID).width / Double(config.width)
                let sy = CGDisplayBounds(d.displayID).height / Double(config.height)
                guard r[0] >= 0, r[1] >= 0, r[2] >= 2, r[3] >= 2,
                      r[0] + r[2] <= config.width, r[1] + r[3] <= config.height else {
                    throw NativeError("区域超出显示器范围")
                }
                config.sourceRect = CGRect(x: Double(r[0]) * sx, y: Double(r[1]) * sy,
                                           width: Double(r[2]) * sx, height: Double(r[3]) * sy)
                config.width = r[2]; config.height = r[3]
            }
        }
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.colorSpaceName = CGColorSpace.sRGB
        config.queueDepth = 3
        config.showsCursor = a["cursor"] as? Bool ?? true
        config.minimumFrameInterval = CMTime(value: 1, timescale: Int32(a["fps"] as? Int ?? 30))
        config.capturesAudio = audioOnly
        config.sampleRate = 48000
        config.channelCount = 2
        // Own playback must also be recorded (e.g. a meeting opened by this process).
        config.excludesCurrentProcessAudio = false
        if audioOnly {
            config.width = 2; config.height = 2
            config.minimumFrameInterval = CMTime(seconds: 1, preferredTimescale: 600)
        }
        let s = SCStream(filter: filter, configuration: config, delegate: self)
        try s.addStreamOutput(self, type: audioOnly ? .audio : .screen, sampleHandlerQueue: queue)
        stream = s
        let done = DispatchSemaphore(value: 0)
        var failure: Error?
        s.startCapture { error in failure = error; done.signal() }
        guard done.wait(timeout: .now() + 10) == .success else { throw NativeError("开始采集超时") }
        if let failure = failure { throw failure }
        stream = s
    }
    func stop() {
        lock.lock(); accepting = false; lock.unlock()
        if let s = stream {
            let done = DispatchSemaphore(value: 0)
            s.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 5)
            try? s.removeStreamOutput(self, type: .screen)
            try? s.removeStreamOutput(self, type: .audio)
            stream = nil
        }
        // Drain all callbacks before Rust releases their context.
        queue.sync {}
    }
    deinit { stop() }
    func stream(_ stream: SCStream, didStopWithError error: Error) { report(error) }
    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        lock.lock(); defer { lock.unlock() }
        guard accepting else { return }
        guard sample.isValid else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        let now = CMClockGetTime(CMClockGetHostTimeClock())
        let latency = max(0, Int64((CMTimeGetSeconds(now) - CMTimeGetSeconds(pts)) * 1_000_000))
        if type == .screen {
            if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
               let status = attachments.first?[.status] as? Int, status != SCFrameStatus.complete.rawValue { return }
            guard let pixel = CMSampleBufferGetImageBuffer(sample) else { return }
            CVPixelBufferLockBaseAddress(pixel, .readOnly)
            defer { CVPixelBufferUnlockBaseAddress(pixel, .readOnly) }
            let stride = CVPixelBufferGetBytesPerRow(pixel), height = CVPixelBufferGetHeight(pixel)
            callback(user, 0, CVPixelBufferGetBaseAddress(pixel), stride * height,
                     Int32(CVPixelBufferGetWidth(pixel)), Int32(height), Int32(stride), latency)
        } else if type == .audio {
            guard let description = sample.formatDescription else { return }
            let format = AVAudioFormat(cmAudioFormatDescription: description)
            let count = CMSampleBufferGetNumSamples(sample)
            guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(count)) else { return }
            buffer.frameLength = AVAudioFrameCount(count)
            let status = CMSampleBufferCopyPCMDataIntoAudioBufferList(sample, at: 0, frameCount: Int32(count), into: buffer.mutableAudioBufferList)
            guard status == noErr, let channels = buffer.floatChannelData else { return }
            var samples = [Float](repeating: 0, count: count * 2)
            for i in 0..<count {
                if format.isInterleaved {
                    samples[i * 2] = channels[0][i * Int(format.channelCount)]
                    samples[i * 2 + 1] = channels[0][i * Int(format.channelCount) + min(1, Int(format.channelCount) - 1)]
                } else {
                    samples[i * 2] = channels[0][i]
                    samples[i * 2 + 1] = channels[min(1, Int(format.channelCount) - 1)][i]
                }
            }
            samples.withUnsafeBytes { callback(user, 1, $0.baseAddress, samples.count, 0, 0, 0, latency) }
        }
    }
}

final class MicrophoneCapture {
    let engine = AVAudioEngine()
    let callback: Callback
    let user: UnsafeMutableRawPointer?
    let queue = DispatchQueue(label: "org.opencap.microphone")
    var tapInstalled = false
    let lock = NSLock()
    var accepting = true
    init(_ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?) {
        self.callback = callback; self.user = user
    }
    func start(_ device: String?) throws {
        let status = AVCaptureDevice.authorizationStatus(for: .audio)
        if status == .notDetermined {
            let done = DispatchSemaphore(value: 0)
            var allowed = false
            AVCaptureDevice.requestAccess(for: .audio) { value in allowed = value; done.signal() }
            guard done.wait(timeout: .now() + 30) == .success, allowed else { throw NativeError("请允许麦克风访问") }
        } else if status != .authorized { throw NativeError("请在系统设置 → 隐私与安全性 → 麦克风中允许 OpenCap") }
        let input = engine.inputNode
        if let uid = device {
            var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
            var size: UInt32 = 0
            guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size) == noErr else { throw NativeError("读取麦克风设备失败") }
            var ids = [AudioDeviceID](repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
            _ = ids.withUnsafeMutableBytes { AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, $0.baseAddress!) }
            var selected: AudioDeviceID?
            for id in ids {
                var a = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyDeviceUID, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
                var value: CFString = "" as CFString
                var s = UInt32(MemoryLayout<CFString>.size)
                if AudioObjectGetPropertyData(id, &a, 0, nil, &s, &value) == noErr, value as String == uid { selected = id; break }
            }
            guard var id = selected, let unit = input.audioUnit else { throw NativeError("所选麦克风已断开") }
            guard AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id, UInt32(MemoryLayout<AudioDeviceID>.size)) == noErr else { throw NativeError("无法切换麦克风") }
        }
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0,
              let target = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 48000, channels: 2, interleaved: false),
              let converter = AVAudioConverter(from: format, to: target) else { throw NativeError("麦克风格式不可用") }
        input.installTap(onBus: 0, bufferSize: 480, format: format) { [weak self] buffer, time in
            guard let self = self else { return }
            self.lock.lock(); defer { self.lock.unlock() }
            guard self.accepting else { return }
            let capacity = AVAudioFrameCount(ceil(Double(buffer.frameLength) * 48000 / format.sampleRate)) + 32
            guard let out = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: capacity) else { return }
            var supplied = false
            var error: NSError?
            let result = converter.convert(to: out, error: &error) { _, status in
                if supplied { status.pointee = .noDataNow; return nil }
                supplied = true; status.pointee = .haveData; return buffer
            }
            guard result != .error, let channels = out.floatChannelData else { return }
            let count = Int(out.frameLength)
            var samples = [Float](repeating: 0, count: count * 2)
            for i in 0..<count { samples[i * 2] = channels[0][i]; samples[i * 2 + 1] = channels[1][i] }
            let host = AVAudioTime.seconds(forHostTime: mach_absolute_time())
            let then = time.isHostTimeValid ? AVAudioTime.seconds(forHostTime: time.hostTime) : host - Double(buffer.frameLength) / format.sampleRate
            let latency = max(0, Int64((host - then) * 1_000_000))
            samples.withUnsafeBytes { self.callback(self.user, 2, $0.baseAddress, samples.count, 0, 0, 0, latency) }
        }
        tapInstalled = true
        try engine.start()
    }
    func stop() {
        lock.lock(); accepting = false; lock.unlock()
        engine.stop()
        if tapInstalled { engine.inputNode.removeTap(onBus: 0); tapInstalled = false }
    }
    deinit { stop() }
}

@_cdecl("oc_start") public func ocStart(_ input: UnsafePointer<CChar>, _ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?, _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> UnsafeMutableRawPointer? {
    let a = arguments(input)
    do {
        if a["microphone"] as? Bool == true {
            let mic = MicrophoneCapture(callback, user)
            try mic.start(a["device"] as? String)
            return Unmanaged.passRetained(mic).toOpaque()
        }
        let capture = StreamCapture(callback, user)
        try capture.start(a)
        return Unmanaged.passRetained(capture).toOpaque()
    } catch { errorOut.pointee = strdup(String(describing: error)); return nil }
}
@_cdecl("oc_stop") public func ocStop(_ handle: UnsafeMutableRawPointer, _ mic: Bool) {
    if mic { Unmanaged<MicrophoneCapture>.fromOpaque(handle).takeRetainedValue().stop() }
    else { Unmanaged<StreamCapture>.fromOpaque(handle).takeRetainedValue().stop() }
}

// All window/status-menu operations below are called on the AppKit main thread.
func window(_ view: Int) -> NSWindow? {
    guard view != 0 else { return nil }
    return Unmanaged<NSView>.fromOpaque(UnsafeRawPointer(bitPattern: view)!).takeUnretainedValue().window
}
@_cdecl("oc_window_id") public func ocWindowID(_ view: Int) -> Int {
    window(view)?.windowNumber ?? 0
}
@_cdecl("oc_window_region") public func ocWindowRegion(_ view: Int) -> UnsafeMutablePointer<CChar>? {
    guard let w = window(view), let s = w.screen else { return jsonString(["error": "窗口不可用"]) }
    let sorted = NSScreen.screens.sorted {
        let a = $0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32 ?? 0
        let b = $1.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32 ?? 0
        if a == b { return false }; if a == CGMainDisplayID() { return true }; if b == CGMainDisplayID() { return false }; return a < b
    }
    let scale = s.backingScaleFactor
    return jsonString(["monitor": sorted.firstIndex(of: s) ?? 0,
                       "x": Int((w.frame.minX - s.frame.minX) * scale), "y": Int((s.frame.maxY - w.frame.maxY) * scale),
                       "width": Int(w.frame.width * scale), "height": Int(w.frame.height * scale)])
}
@_cdecl("oc_window") public func ocWindow(_ view: Int, _ action: Int32, _ value: Bool) -> Bool {
    guard let w = window(view) else { return false }
    if action == 4 { return w.isVisible }
    // GPUI may hold an App borrow here. Defer AppKit callbacks to the next turn.
    DispatchQueue.main.async {
        switch action {
        case 0: if value { w.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true) } else { w.orderOut(nil) }
        case 1: if value { w.orderFrontRegardless() } else { w.orderOut(nil) }
        case 2: w.sharingType = value ? .none : .readOnly
        case 3: w.level = .floating; w.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]; w.isMovableByWindowBackground = true
        default: break
        }
    }
    return true
}

final class DesktopMenu: NSObject {
    let callback: Callback
    let user: UnsafeMutableRawPointer?
    let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    var keys = [EventHotKeyRef]()
    var handler: EventHandlerRef?
    var notifications = [NSObjectProtocol]()
    init(_ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?) {
        self.callback = callback; self.user = user
        super.init()
        item.button?.toolTip = "OpenCap"
        state(false)
        let menu = NSMenu()
        for (index, title) in ["显示 OpenCap", "开始 / 停止", "暂停 / 继续", "打开输出目录", "退出"].enumerated() {
            let entry = NSMenuItem(title: title, action: #selector(action(_:)), keyEquivalent: "")
            entry.tag = index; entry.target = self; menu.addItem(entry)
        }
        item.menu = menu
        let center = DistributedNotificationCenter.default()
        for name in ["com.apple.screenIsLocked", "com.apple.sessionDidResignActive"] {
            notifications.append(center.addObserver(forName: NSNotification.Name(name), object: nil, queue: .main) { [weak self] _ in self?.send(5) })
        }
    }
    func send(_ event: Int32) { callback(user, event, nil, 0, 0, 0, 0, 0) }
    @objc func action(_ item: NSMenuItem) { send(Int32(item.tag)) }
    func state(_ recording: Bool) {
        item.button?.image = NSImage(systemSymbolName: recording ? "record.circle.fill" : "record.circle", accessibilityDescription: "OpenCap")
        item.button?.contentTintColor = recording ? .systemRed : .systemGreen
    }
    func register(_ a: [String: Any]) throws {
        var type = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let pointer = Unmanaged.passUnretained(self).toOpaque()
        let status = InstallEventHandler(GetApplicationEventTarget(), { _, event, user in
            guard let user = user, let event = event else { return OSStatus(eventNotHandledErr) }
            var id = EventHotKeyID()
            let status = GetEventParameter(event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID), nil, MemoryLayout<EventHotKeyID>.size, nil, &id)
            if status == noErr { Unmanaged<DesktopMenu>.fromOpaque(user).takeUnretainedValue().send(Int32(id.id)) }
            return status
        }, 1, &type, pointer, &handler)
        guard status == noErr else { throw NativeError("注册快捷键处理器失败") }
        let functionKeys: [UInt32] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90]
        for (name, id) in [("record", UInt32(1)), ("pause", UInt32(2))] {
            let text = a[name] as? String ?? ""
            let parts = text.lowercased().split(separator: "+").map(String.init)
            guard let fn = parts.last, fn.hasPrefix("f"), let number = Int(fn.dropFirst()), (1...20).contains(number) else { throw NativeError("快捷键需包含 F1–F20") }
            var modifiers: UInt32 = 0
            for part in parts.dropLast() {
                switch part {
                case "ctrl", "control": modifiers |= UInt32(controlKey)
                case "alt", "option": modifiers |= UInt32(optionKey)
                case "shift": modifiers |= UInt32(shiftKey)
                case "cmd", "command": modifiers |= UInt32(cmdKey)
                default: throw NativeError("不支持的快捷键修饰符：\(part)")
                }
            }
            var key: EventHotKeyRef?
            let result = RegisterEventHotKey(functionKeys[number - 1], modifiers, EventHotKeyID(signature: 0x4F434150, id: id), GetApplicationEventTarget(), 0, &key)
            guard result == noErr, let key = key else { throw NativeError("快捷键 \(text) 被其他应用占用，请切换组合") }
            keys.append(key)
        }
    }
    deinit {
        for key in keys { UnregisterEventHotKey(key) }
        if let handler = handler { RemoveEventHandler(handler) }
        for token in notifications { DistributedNotificationCenter.default().removeObserver(token) }
        NSStatusBar.system.removeStatusItem(item)
    }
}
@_cdecl("oc_controls") public func ocControls(_ input: UnsafePointer<CChar>, _ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?, _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> UnsafeMutableRawPointer? {
    do {
        let menu = DesktopMenu(callback, user)
        try menu.register(arguments(input))
        return Unmanaged.passRetained(menu).toOpaque()
    } catch { errorOut.pointee = strdup(String(describing: error)); return nil }
}
@_cdecl("oc_controls_state") public func ocControlsState(_ handle: UnsafeMutableRawPointer, _ value: Bool) {
    Unmanaged<DesktopMenu>.fromOpaque(handle).takeUnretainedValue().state(value)
}
@_cdecl("oc_controls_drop") public func ocControlsDrop(_ handle: UnsafeMutableRawPointer) {
    _ = Unmanaged<DesktopMenu>.fromOpaque(handle).takeRetainedValue()
}

// Physical-pixel coordinates in Rust are converted only at the AppKit boundary.
func screen(_ index: Int) -> NSScreen? {
    let list = NSScreen.screens.sorted {
        let a = ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32) ?? 0
        let b = ($1.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32) ?? 0
        if a == b { return false }
        if a == CGMainDisplayID() { return true }; if b == CGMainDisplayID() { return false }; return a < b
    }
    return list.indices.contains(index) ? list[index] : nil
}
final class RegionView: NSView {
    let callback: Callback
    let user: UnsafeMutableRawPointer?
    let scale: CGFloat
    var selection: CGRect?
    var origin = CGPoint.zero
    var original = CGRect.zero
    var handle = -1
    let start = NSButton(title: "开始录制", target: nil, action: nil)
    let cancel = NSButton(title: "取消 (Esc)", target: nil, action: nil)
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    init(_ frame: CGRect, _ scale: CGFloat, _ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?, _ initial: [String: Any]?) {
        self.scale = scale; self.callback = callback; self.user = user
        super.init(frame: frame)
        if let r = initial, let x = r["x"] as? Double, let y = r["y"] as? Double, let w = r["width"] as? Double, let h = r["height"] as? Double {
            selection = CGRect(x: x / scale, y: y / scale, width: w / scale, height: h / scale)
        }
        start.target = self; start.action = #selector(confirm); start.bezelStyle = .rounded
        cancel.target = self; cancel.action = #selector(close); cancel.bezelStyle = .rounded
        addSubview(start); addSubview(cancel); controls()
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unsupported") }
    func controls() {
        start.isEnabled = selection != nil
        let r = selection ?? CGRect(x: 20, y: 20, width: 0, height: 0)
        let x = min(max(10, r.minX), max(10, bounds.width - 270))
        let y = min(max(10, r.maxY + 10), bounds.height - 45)
        start.frame = CGRect(x: x, y: y, width: 125, height: 32)
        cancel.frame = CGRect(x: x + 135, y: y, width: 125, height: 32)
        needsDisplay = true
    }
    func send(_ type: Int32) {
        let r = selection ?? .zero
        callback(user, type, nil, 0, Int32((r.minX * scale).rounded()), Int32((r.minY * scale).rounded()), Int32((r.width * scale).rounded()), Int64((r.height * scale).rounded()))
    }
    @objc func confirm() { guard selection != nil else { return }; send(1); window?.orderOut(nil) }
    @objc func close() { send(2); window?.orderOut(nil) }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 { close() } else if event.keyCode == 36 { confirm() }
    }
    override func rightMouseDown(with event: NSEvent) { close() }
    override func mouseDown(with event: NSEvent) {
        origin = convert(event.locationInWindow, from: nil); original = selection ?? .zero; handle = -1
        if let r = selection {
            let left = abs(origin.x - r.minX) < 8, right = abs(origin.x - r.maxX) < 8
            let top = abs(origin.y - r.minY) < 8, bottom = abs(origin.y - r.maxY) < 8
            if r.insetBy(dx: -8, dy: -8).contains(origin) {
                handle = (left ? 1 : 0) | (right ? 2 : 0) | (top ? 4 : 0) | (bottom ? 8 : 0)
            }
        }
    }
    override func mouseDragged(with event: NSEvent) {
        var p = convert(event.locationInWindow, from: nil)
        p.x = min(max(0, p.x), bounds.width); p.y = min(max(0, p.y), bounds.height)
        let dx = p.x - origin.x, dy = p.y - origin.y
        var r: CGRect
        if handle == -1 { r = CGRect(x: min(origin.x, p.x), y: min(origin.y, p.y), width: abs(dx), height: abs(dy)) }
        else if handle == 0 {
            r = original; r.origin.x = min(max(0, r.origin.x + dx), bounds.width - r.width)
            r.origin.y = min(max(0, r.origin.y + dy), bounds.height - r.height)
        } else {
            var x1 = original.minX, x2 = original.maxX, y1 = original.minY, y2 = original.maxY
            if handle & 1 != 0 { x1 = min(p.x, x2 - 2 / scale) }; if handle & 2 != 0 { x2 = max(p.x, x1 + 2 / scale) }
            if handle & 4 != 0 { y1 = min(p.y, y2 - 2 / scale) }; if handle & 8 != 0 { y2 = max(p.y, y1 + 2 / scale) }
            r = CGRect(x: x1, y: y1, width: x2 - x1, height: y2 - y1)
        }
        selection = r.width * scale >= 2 && r.height * scale >= 2 ? r : nil; controls(); send(0)
    }
    override func mouseUp(with event: NSEvent) { controls(); send(0) }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.black.withAlphaComponent(0.25).setFill(); bounds.fill()
        guard let r = selection else { return }
        NSColor.clear.setFill(); r.fill(using: .copy)
        NSColor.systemRed.setStroke(); let path = NSBezierPath(rect: r); path.lineWidth = 2; path.stroke()
        for p in [CGPoint(x: r.minX, y: r.minY), CGPoint(x: r.maxX, y: r.minY), CGPoint(x: r.minX, y: r.maxY), CGPoint(x: r.maxX, y: r.maxY)] {
            NSColor.white.setFill(); CGRect(x: p.x - 4, y: p.y - 4, width: 8, height: 8).fill()
        }
    }
}
final class SelectorWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}
@_cdecl("oc_region") public func ocRegion(_ input: UnsafePointer<CChar>, _ callback: @escaping Callback, _ user: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    let a = arguments(input)
    guard let s = screen(a["monitor"] as? Int ?? 0) else { return nil }
    let w = SelectorWindow(contentRect: s.frame, styleMask: [.borderless], backing: .buffered, defer: false)
    w.isReleasedWhenClosed = false; w.isOpaque = false; w.backgroundColor = .clear
    w.level = .screenSaver; w.sharingType = .none
    w.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
    w.contentView = RegionView(CGRect(origin: .zero, size: s.frame.size), s.backingScaleFactor, callback, user, a["initial"] as? [String: Any])
    w.makeKeyAndOrderFront(nil); w.makeFirstResponder(w.contentView); NSApp.activate(ignoringOtherApps: true)
    return Unmanaged.passRetained(w).toOpaque()
}
@_cdecl("oc_region_drop") public func ocRegionDrop(_ handle: UnsafeMutableRawPointer) {
    let w = Unmanaged<NSWindow>.fromOpaque(handle).takeRetainedValue(); w.orderOut(nil); w.contentView = nil; w.close()
}

final class BorderView: NSView {
    var paused = false
    override func draw(_ dirtyRect: NSRect) {
        (paused ? NSColor.systemOrange : NSColor.systemRed).setStroke()
        let path = NSBezierPath(rect: bounds.insetBy(dx: 2, dy: 2)); path.lineWidth = 3; path.stroke()
    }
}
@_cdecl("oc_indicator") public func ocIndicator(_ input: UnsafePointer<CChar>, _ handle: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    let a = arguments(input), t = a["target"] as? [String: Any] ?? [:]
    var frame = CGRect.zero
    if let index = t["Monitor"] as? Int, let s = screen(index) { frame = s.frame }
    else if let r = t["Region"] as? [String: Any], let index = r["monitor"] as? Int, let s = screen(index), let rect = r["rect"] as? [String: Double] {
        let scale = s.backingScaleFactor
        frame = CGRect(x: s.frame.minX + (rect["x"] ?? 0) / scale,
                       y: s.frame.maxY - ((rect["y"] ?? 0) + (rect["height"] ?? 0)) / scale,
                       width: (rect["width"] ?? 0) / scale, height: (rect["height"] ?? 0) / scale)
    } else if let id = t["Window"] as? UInt32,
              let list = CGWindowListCopyWindowInfo([.optionIncludingWindow], id) as? [[String: Any]],
              list.first?[kCGWindowIsOnscreen as String] as? Bool == true,
              let bounds = list.first?[kCGWindowBounds as String] as? [String: Any],
              let r = CGRect(dictionaryRepresentation: bounds as CFDictionary) {
        frame = CGRect(x: r.minX, y: (NSScreen.screens.first?.frame.maxY ?? 0) - r.maxY, width: r.width, height: r.height)
    }
    let w: NSWindow
    if let handle = handle { w = Unmanaged<NSWindow>.fromOpaque(handle).takeUnretainedValue() }
    else {
        w = NSWindow(contentRect: frame, styleMask: .borderless, backing: .buffered, defer: false)
        w.isReleasedWhenClosed = false; w.isOpaque = false; w.backgroundColor = .clear
        w.level = .floating; w.ignoresMouseEvents = true; w.sharingType = .none
        w.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]; w.contentView = BorderView()
    }
    if frame.width > 0 { w.setFrame(frame, display: true); w.orderFrontRegardless() } else { w.orderOut(nil) }
    (w.contentView as? BorderView)?.paused = a["paused"] as? Bool ?? false; w.contentView?.needsDisplay = true
    return handle ?? Unmanaged.passRetained(w).toOpaque()
}
@_cdecl("oc_indicator_drop") public func ocIndicatorDrop(_ handle: UnsafeMutableRawPointer) {
    let w = Unmanaged<NSWindow>.fromOpaque(handle).takeRetainedValue(); w.orderOut(nil); w.close()
}
@_cdecl("oc_indicator_visible") public func ocIndicatorVisible(_ handle: UnsafeMutableRawPointer) -> Bool {
    Unmanaged<NSWindow>.fromOpaque(handle).takeUnretainedValue().isVisible
}

final class PreviewRoot: NSView {
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
}
final class Preview {
    let root = PreviewRoot()
    let image = NSImageView()
    let source: UInt32
    var requested = false
    init(_ destination: NSView, _ source: UInt32) {
        self.source = source; root.wantsLayer = true; root.layer?.masksToBounds = true
        image.imageScaling = .scaleProportionallyUpOrDown; root.addSubview(image); destination.addSubview(root)
    }
    func place(_ a: [String: Any]) {
        guard let area = a["area"] as? [CGFloat], let clip = a["clip"] as? [CGFloat], area.count == 4, clip.count == 4,
              let view = root.superview, let w = view.window else { return }
        let scale = w.backingScaleFactor
        let full = CGRect(x: area[0] / scale, y: view.bounds.height - area[3] / scale, width: (area[2] - area[0]) / scale, height: (area[3] - area[1]) / scale)
        let mask = CGRect(x: clip[0] / scale, y: view.bounds.height - clip[3] / scale, width: (clip[2] - clip[0]) / scale, height: (clip[3] - clip[1]) / scale)
        let visible = full.intersection(mask)
        root.isHidden = visible.isNull || visible.isEmpty
        guard !root.isHidden else { return }
        root.frame = visible; image.frame = CGRect(x: full.minX - visible.minX, y: full.minY - visible.minY, width: full.width, height: full.height)
        guard !requested else { return }; requested = true
        if #available(macOS 14.0, *) {
            SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { [weak self] c, _ in
                guard let self = self, let source = c?.windows.first(where: { $0.windowID == self.source }) else { return }
                let config = SCStreamConfiguration(); config.width = 320; config.height = max(2, Int(320 * source.frame.height / source.frame.width))
                SCScreenshotManager.captureImage(contentFilter: SCContentFilter(desktopIndependentWindow: source), configuration: config) { [weak self] cg, _ in
                    guard let cg = cg else { return }
                    DispatchQueue.main.async { self?.image.image = NSImage(cgImage: cg, size: .zero) }
                }
            }
        }
    }
    deinit { root.removeFromSuperview() }
}
@_cdecl("oc_preview") public func ocPreview(_ view: Int, _ source: UInt32) -> UnsafeMutableRawPointer? {
    guard #available(macOS 14.0, *) else { return nil }
    guard view != 0 else { return nil }
    let destination = Unmanaged<NSView>.fromOpaque(UnsafeRawPointer(bitPattern: view)!).takeUnretainedValue()
    return Unmanaged.passRetained(Preview(destination, source)).toOpaque()
}
@_cdecl("oc_preview_place") public func ocPreviewPlace(_ handle: UnsafeMutableRawPointer, _ input: UnsafePointer<CChar>) {
    Unmanaged<Preview>.fromOpaque(handle).takeUnretainedValue().place(arguments(input))
}
@_cdecl("oc_preview_drop") public func ocPreviewDrop(_ handle: UnsafeMutableRawPointer) { _ = Unmanaged<Preview>.fromOpaque(handle).takeRetainedValue() }

final class FixtureCanvas {
    let width: Int, height: Int
    let data: UnsafeMutableRawPointer
    let context: CGContext
    init?(_ width: Int, _ height: Int) {
        guard width > 0, height > 0 else { return nil }
        self.width = width; self.height = height
        data = UnsafeMutableRawPointer.allocate(byteCount: width * height * 4, alignment: 16)
        data.initializeMemory(as: UInt8.self, repeating: 0, count: width * height * 4)
        guard let c = CGContext(data: data, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                                space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.premultipliedFirst.rawValue) else { data.deallocate(); return nil }
        context = c
    }
    func color(_ value: UInt32) -> CGColor {
        CGColor(red: CGFloat(value & 255) / 255, green: CGFloat((value >> 8) & 255) / 255, blue: CGFloat((value >> 16) & 255) / 255, alpha: 1)
    }
    deinit { data.deallocate() }
}
@_cdecl("oc_canvas") public func ocCanvas(_ width: Int32, _ height: Int32) -> UnsafeMutableRawPointer? {
    guard let c = FixtureCanvas(Int(width), Int(height)) else { return nil }; return Unmanaged.passRetained(c).toOpaque()
}
@_cdecl("oc_canvas_rect") public func ocCanvasRect(_ handle: UnsafeMutableRawPointer, _ x: Int32, _ y: Int32, _ width: Int32, _ height: Int32, _ color: UInt32) {
    let c = Unmanaged<FixtureCanvas>.fromOpaque(handle).takeUnretainedValue()
    c.context.setFillColor(c.color(color)); c.context.fill(CGRect(x: Int(x), y: c.height - Int(y + height), width: Int(width), height: Int(height)))
}
@_cdecl("oc_canvas_text") public func ocCanvasText(_ handle: UnsafeMutableRawPointer, _ x: Int32, _ y: Int32, _ size: Int32, _ color: UInt32, _ text: UnsafePointer<CChar>) {
    let c = Unmanaged<FixtureCanvas>.fromOpaque(handle).takeUnretainedValue()
    let font = CTFontCreateWithName("PingFang SC" as CFString, CGFloat(size), nil)
    let value = NSAttributedString(string: String(cString: text), attributes: [NSAttributedString.Key(kCTFontAttributeName as String): font, NSAttributedString.Key(kCTForegroundColorAttributeName as String): c.color(color)])
    c.context.textMatrix = .identity; c.context.textPosition = CGPoint(x: Int(x), y: c.height - Int(y) - Int(size))
    CTLineDraw(CTLineCreateWithAttributedString(value), c.context)
}
@_cdecl("oc_canvas_data") public func ocCanvasData(_ handle: UnsafeMutableRawPointer) -> UnsafeRawPointer {
    UnsafeRawPointer(Unmanaged<FixtureCanvas>.fromOpaque(handle).takeUnretainedValue().data)
}
@_cdecl("oc_canvas_drop") public func ocCanvasDrop(_ handle: UnsafeMutableRawPointer) { _ = Unmanaged<FixtureCanvas>.fromOpaque(handle).takeRetainedValue() }
