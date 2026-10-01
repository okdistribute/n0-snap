#import <UIKit/UIKit.h>
#import <AVFoundation/AVFoundation.h>
#import <PhotosUI/PhotosUI.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#import <SafariServices/SafariServices.h>

typedef void (*SnapCallback)(void *, int, const char *);
static UIViewController *topController(void) {
    for (UIScene *scene in UIApplication.sharedApplication.connectedScenes) {
        if (![scene isKindOfClass:UIWindowScene.class] || scene.activationState != UISceneActivationStateForegroundActive) continue;
        for (UIWindow *window in ((UIWindowScene *)scene).windows) {
            if (!window.isKeyWindow) continue;
            UIViewController *controller = window.rootViewController;
            while (controller.presentedViewController) controller = controller.presentedViewController;
            return controller;
        }
    }
    // Tao currently uses the UIApplicationDelegate lifecycle, without scenes.
    UIViewController *controller = UIApplication.sharedApplication.keyWindow.rootViewController;
    while (controller.presentedViewController) controller = controller.presentedViewController;
    return controller;
}

@interface SnapOperation : NSObject
@property(nonatomic, assign) void *context;
@property(nonatomic, assign) SnapCallback callback;
@property(nonatomic, assign) BOOL finished;
- (void)finish:(int)status text:(NSString *)text;
@end
@implementation SnapOperation
- (void)finish:(int)status text:(NSString *)text {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (self.finished) return;
        self.finished = YES;
        self.callback(self.context, status, text.UTF8String);
    });
}
@end

@interface SnapScanner : UIViewController <AVCaptureMetadataOutputObjectsDelegate>
@property(nonatomic, strong) SnapOperation *operation;
@property(nonatomic, strong) AVCaptureSession *session;
@property(nonatomic, strong) AVCaptureVideoPreviewLayer *preview;
@property(nonatomic, strong) dispatch_queue_t cameraQueue;
@property(nonatomic, assign) BOOL completed;
@end
@implementation SnapScanner
- (void)viewDidLoad {
    [super viewDidLoad];
    self.view.backgroundColor = UIColor.blackColor;
    self.cameraQueue = dispatch_queue_create("dev.flicker.camera", DISPATCH_QUEUE_SERIAL);
    UIButton *cancel = [UIButton buttonWithType:UIButtonTypeSystem];
    [cancel setTitle:@"Cancel" forState:UIControlStateNormal];
    [cancel addTarget:self action:@selector(cancel) forControlEvents:UIControlEventTouchUpInside];
    cancel.translatesAutoresizingMaskIntoConstraints = NO;
    [self.view addSubview:cancel];
    UILabel *hint = [UILabel new];
    hint.text = @"Point at your friend's Snapcode";
    hint.textColor = UIColor.whiteColor;
    hint.translatesAutoresizingMaskIntoConstraints = NO;
    [self.view addSubview:hint];
    [NSLayoutConstraint activateConstraints:@[
        [cancel.topAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.topAnchor constant:12],
        [cancel.trailingAnchor constraintEqualToAnchor:self.view.trailingAnchor constant:-20],
        [cancel.heightAnchor constraintEqualToConstant:44],
        [hint.bottomAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.bottomAnchor constant:-32],
        [hint.centerXAnchor constraintEqualToAnchor:self.view.centerXAnchor]
    ]];
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
        dispatch_async(dispatch_get_main_queue(), ^{
            if (self.completed) return;
            if (!granted) { [self done:2 text:@"Camera access is off. Enable it in Settings, or paste a Snapcode."]; return; }
            [self configureCamera];
        });
    }];
}
- (void)configureCamera {
    NSError *error = nil;
    AVCaptureDevice *device = [AVCaptureDevice defaultDeviceWithMediaType:AVMediaTypeVideo];
    if (!device) { [self done:2 text:@"No camera available. Paste a Snapcode instead."]; return; }
    AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device error:&error];
    AVCaptureSession *session = [AVCaptureSession new];
    AVCaptureMetadataOutput *output = [AVCaptureMetadataOutput new];
    if (!input || ![session canAddInput:input]) { [self done:2 text:@"Couldn't start the camera."]; return; }
    [session addInput:input];
    if (![session canAddOutput:output]) { [self done:2 text:@"QR scanning isn't available."]; return; }
    [session addOutput:output];
    [output setMetadataObjectsDelegate:self queue:dispatch_get_main_queue()];
    if (![output.availableMetadataObjectTypes containsObject:AVMetadataObjectTypeQRCode]) { [self done:2 text:@"QR scanning isn't available."]; return; }
    output.metadataObjectTypes = @[AVMetadataObjectTypeQRCode];
    self.session = session;
    self.preview = [AVCaptureVideoPreviewLayer layerWithSession:session];
    self.preview.videoGravity = AVLayerVideoGravityResizeAspectFill;
    self.preview.frame = self.view.bounds;
    [self.view.layer insertSublayer:self.preview atIndex:0];
    dispatch_async(self.cameraQueue, ^{ [session startRunning]; });
}
- (void)viewDidLayoutSubviews { [super viewDidLayoutSubviews]; self.preview.frame = self.view.bounds; }
- (void)cancel { [self done:1 text:nil]; }
- (void)done:(int)status text:(NSString *)text {
    if (self.completed) return;
    self.completed = YES;
    AVCaptureSession *session = self.session;
    if (session) dispatch_async(self.cameraQueue, ^{ [session stopRunning]; });
    [self dismissViewControllerAnimated:YES completion:^{ [self.operation finish:status text:text]; }];
}
- (void)metadataOutput:(AVCaptureMetadataOutput *)output didOutputMetadataObjects:(NSArray<__kindof AVMetadataObject *> *)objects fromConnection:(AVCaptureConnection *)connection {
    for (AVMetadataObject *object in objects) {
        if (![object isKindOfClass:AVMetadataMachineReadableCodeObject.class]) continue;
        NSString *text = ((AVMetadataMachineReadableCodeObject *)object).stringValue;
        if ([text hasPrefix:@"flicker://friend/"] && text.length < 16000) { [self done:0 text:text]; return; }
    }
}
@end

static id pickerDelegate;
@interface SnapPicker : SnapOperation <PHPickerViewControllerDelegate, UIAdaptivePresentationControllerDelegate>
@end
@implementation SnapPicker
- (void)finish:(int)status text:(NSString *)text {
    [super finish:status text:text];
    dispatch_async(dispatch_get_main_queue(), ^{ if (pickerDelegate == self) pickerDelegate = nil; });
}
- (void)presentationControllerDidDismiss:(UIPresentationController *)controller { [self finish:1 text:nil]; }
- (void)picker:(PHPickerViewController *)picker didFinishPicking:(NSArray<PHPickerResult *> *)results {
    [picker dismissViewControllerAnimated:YES completion:nil];
    if (!results.count) { [self finish:1 text:nil]; return; }
    NSItemProvider *provider = results.firstObject.itemProvider;
    BOOL movie = [provider hasItemConformingToTypeIdentifier:UTTypeMovie.identifier];
    NSString *type = movie ? UTTypeMovie.identifier : UTTypeImage.identifier;
    [provider loadFileRepresentationForTypeIdentifier:type completionHandler:^(NSURL *url, NSError *error) {
        @autoreleasepool {
            if (!url) { [self finish:2 text:error.localizedDescription ?: @"Couldn't load this photo or video."]; return; }
            NSNumber *size = nil;
            [url getResourceValue:&size forKey:NSURLFileSizeKey error:nil];
            if (size.unsignedLongLongValue > 12 * 1024 * 1024) { [self finish:2 text:@"Choose media smaller than 12 MB."]; return; }
            NSData *data = [NSData dataWithContentsOfURL:url options:NSDataReadingMappedIfSafe error:&error];
            NSString *mime = [UTType typeWithFilenameExtension:url.pathExtension].preferredMIMEType;
            NSString *name = url.lastPathComponent;
            if (!movie && ![@[@"image/jpeg", @"image/png", @"image/gif", @"image/webp"] containsObject:mime ?: @""]) {
                UIImage *image = [UIImage imageWithData:data];
                data = image ? UIImageJPEGRepresentation(image, 0.9) : nil;
                mime = @"image/jpeg"; name = @"Photo.jpg";
            }
            if (!data || !mime) { [self finish:2 text:@"Couldn't read this media format."]; return; }
            if (data.length > 12 * 1024 * 1024) { [self finish:2 text:@"Choose media smaller than 12 MB."]; return; }
            NSData *json = [NSJSONSerialization dataWithJSONObject:@{@"data":[data base64EncodedStringWithOptions:0], @"mime":mime, @"name":name} options:0 error:nil];
            [self finish:0 text:[[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding]];
        }
    }];
}
@end

void snap_scan_qr(void *context, SnapCallback callback) {
    dispatch_async(dispatch_get_main_queue(), ^{
        SnapOperation *operation = [SnapOperation new]; operation.context = context; operation.callback = callback;
        UIViewController *top = topController();
        if (!top) { [operation finish:2 text:@"App isn't ready for the camera."]; return; }
        SnapScanner *scanner = [SnapScanner new]; scanner.operation = operation;
        scanner.modalPresentationStyle = UIModalPresentationFullScreen;
        [top presentViewController:scanner animated:YES completion:nil];
    });
}
void snap_pick_media(void *context, SnapCallback callback) {
    dispatch_async(dispatch_get_main_queue(), ^{
        SnapPicker *delegate = [SnapPicker new]; delegate.context = context; delegate.callback = callback;
        UIViewController *top = topController();
        if (!top || pickerDelegate) { [delegate finish:2 text:@"Photo picker is already open or unavailable."]; return; }
        pickerDelegate = delegate;
        PHPickerConfiguration *config = [PHPickerConfiguration new];
        config.filter = [PHPickerFilter anyFilterMatchingSubfilters:@[PHPickerFilter.imagesFilter, PHPickerFilter.videosFilter]];
        config.selectionLimit = 1;
        PHPickerViewController *picker = [[PHPickerViewController alloc] initWithConfiguration:config];
        picker.delegate = delegate;
        picker.presentationController.delegate = delegate;
        [top presentViewController:picker animated:YES completion:nil];
    });
}

// Safari's system UI keeps the native loopback listener alive during the demo
// login; the app never sees the page or the user's password.
static SFSafariViewController *authBrowser;
void snap_browser_open(const char *url) {
    NSString *text = [NSString stringWithUTF8String:url];
    dispatch_async(dispatch_get_main_queue(), ^{
        if (authBrowser.presentingViewController) return;
        UIViewController *top = topController();
        if (!top) return;
        authBrowser = [[SFSafariViewController alloc] initWithURL:[NSURL URLWithString:text]];
        authBrowser.modalPresentationStyle = UIModalPresentationFullScreen;
        [top presentViewController:authBrowser animated:YES completion:nil];
    });
}
void snap_browser_close(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        [authBrowser dismissViewControllerAnimated:YES completion:nil];
        authBrowser = nil;
    });
}
