import AVFoundation
import UIKit

/// Which way the camera turns the picture, in one place for iOS 17+ and iOS 16 (iPhone 8, X).
///
/// iOS 17 has `AVCaptureDevice.RotationCoordinator` and `videoRotationAngle`; iOS 16 has neither.
/// There the device orientation (the accelerometer, through `UIDevice`) gives the same four angles.
/// `-legacyRotation YES` on the launch arguments runs the iOS 16 path on a newer iPhone, for tests.
enum HorizonRotation {
    static var legacy: Bool {
        if #available(iOS 17.0, *) { return UserDefaults.standard.bool(forKey: "legacyRotation") }
        return true
    }

    /// Frames stay in the sensor's own orientation (landscape, angle 0); the PC turns them.
    static func keepSensorOrientation(_ connection: AVCaptureConnection) {
        if #available(iOS 17.0, *), !legacy {
            if connection.isVideoRotationAngleSupported(0) { connection.videoRotationAngle = 0 }
        } else if connection.isVideoOrientationSupported {
            connection.videoOrientation = .landscapeRight
        }
    }

    /// Clockwise degrees that make a sensor-oriented frame upright for a phone held like this
    /// (the camera's landscape sensor is "landscape right" for AVFoundation, the home button on the right).
    /// nil while the phone lies flat or the orientation is not known.
    static func degrees(for orientation: UIDeviceOrientation) -> Int? {
        switch orientation {
        case .portrait: return 90
        case .portraitUpsideDown: return 270
        case .landscapeLeft: return 0      // UIDeviceOrientation and AVCaptureVideoOrientation name the two landscapes oppositely
        case .landscapeRight: return 180
        default: return nil
        }
    }
}

/// iOS 16: the phone's orientation from `UIDevice`. It is switched on at launch and never off, so
/// the first reading of a stream is already there (right after switching on it is still unknown).
/// The value is for the back camera; `CaptureRotation` adjusts it for the front one.
enum DeviceOrientation {
    static let changed = Notification.Name("vn.camstudio.deviceOrientationChanged")

    private static let lock = NSLock()
    private static var degrees = 90                 // portrait until the phone says otherwise
    private static var started = false              // main thread only

    /// Main thread; calling it again does nothing.
    static func start() {
        guard !started else { return }
        started = true
        UIDevice.current.beginGeneratingDeviceOrientationNotifications()
        NotificationCenter.default.addObserver(forName: UIDevice.orientationDidChangeNotification, object: nil, queue: .main) { _ in read() }
        read()
    }

    /// Any thread.
    static var current: Int {
        lock.lock()
        defer { lock.unlock() }
        return degrees
    }

    private static func read() {
        guard let new = HorizonRotation.degrees(for: UIDevice.current.orientation) else { return }
        lock.lock()
        let different = new != degrees
        degrees = new
        lock.unlock()
        if different { NotificationCenter.default.post(name: changed, object: nil) }
    }
}

/// The angle the PC must turn the camera frames by to see them upright (`state.orientation`).
/// Readable from any thread; `onChange` runs when the angle changes.
final class CaptureRotation {
    private let onChange: () -> Void
    private let front: Bool
    private var coordinator: AnyObject?      // AVCaptureDevice.RotationCoordinator (iOS 17+)
    private var observer: NSKeyValueObservation?
    private var token: NSObjectProtocol?

    init(device: AVCaptureDevice, onChange: @escaping () -> Void) {
        self.onChange = onChange
        front = device.position == .front
        if #available(iOS 17.0, *), !HorizonRotation.legacy {
            let c = AVCaptureDevice.RotationCoordinator(device: device, previewLayer: nil)
            coordinator = c
            observer = c.observe(\.videoRotationAngleForHorizonLevelCapture, options: [.new]) { [weak self] _, _ in
                self?.onChange()
            }
        } else {
            DispatchQueue.main.async { DeviceOrientation.start() }
            token = NotificationCenter.default.addObserver(forName: DeviceOrientation.changed, object: nil, queue: nil) { [weak self] _ in
                self?.onChange()
            }
        }
    }

    var angle: Int {
        if #available(iOS 17.0, *), let c = coordinator as? AVCaptureDevice.RotationCoordinator {
            return Int(c.videoRotationAngleForHorizonLevelCapture.rounded()) % 360
        }
        let back = DeviceOrientation.current
        // Front frames come mirrored, which turns an angle `a` into `180 - a`: the portraits stay,
        // the two landscapes swap (the angles RotationCoordinator gives, measured on an iPhone 14).
        return front ? (540 - back) % 360 : back
    }

    func invalidate() {
        observer = nil
        coordinator = nil
        if let token { NotificationCenter.default.removeObserver(token) }
        token = nil
    }
}
