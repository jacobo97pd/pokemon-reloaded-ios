// The UIKit side of the iOS app.
//
// It owns everything that is Objective-C: the window, the OpenGL ES layer and context, the
// keyboard, the audio session and the app's lifecycle. The engine (Rust, src/ios_main.rs) runs on
// a thread of its own; this file forwards touches, typed text, size changes and the
// foreground/background switch to it, and implements the rl_host_* functions it calls back.

#define GLES_SILENCE_DEPRECATION 1

#import <AVFoundation/AVFoundation.h>
#import <OpenGLES/EAGL.h>
#import <OpenGLES/ES3/gl.h>
#import <QuartzCore/QuartzCore.h>
#import <UIKit/UIKit.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

// Engine entry points (src/OpenGMK/gm8emulator/src/ios_main.rs).
extern void rl_ios_start(const char *documents, const char *bundle, void *layer, uint32_t width, uint32_t height);
extern void rl_ios_resize(uint32_t width, uint32_t height);
extern void rl_ios_set_active(bool active);
extern void rl_ios_audio_interrupted(bool interrupted);
extern void rl_ios_touch(int32_t phase, int32_t touch_id, float x, float y);
extern void rl_ios_tick(void);
extern void rl_ios_commit_text(const char *text);
extern void rl_ios_backspace(void);
extern void rl_ios_enter(void);
extern void rl_ios_ime_inset(uint32_t height);

@class GameView;
@class GameViewController;
static GameView *gGameView;
static GameViewController *gController;

// -------------------------------------------------------------------------------------------------
// OpenGL ES. Everything here runs on the engine's render thread, which owns the context.

static EAGLContext *gContext;
static CAEAGLLayer *gLayer;
static GLuint gFramebuffer, gColorBuffer, gDepthBuffer;

// (Re)allocates the screen framebuffer's storage at the layer's current size. Leaves the screen
// framebuffer and the colour renderbuffer bound.
static bool allocate_storage(void) {
    glBindRenderbuffer(GL_RENDERBUFFER, gColorBuffer);
    if (![gContext renderbufferStorage:GL_RENDERBUFFER fromDrawable:gLayer]) {
        fprintf(stderr, "[host] renderbufferStorage:fromDrawable: fallo\n");
        return false;
    }
    GLint width = 0, height = 0;
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_WIDTH, &width);
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_HEIGHT, &height);
    glBindRenderbuffer(GL_RENDERBUFFER, gDepthBuffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, width, height);
    glBindFramebuffer(GL_FRAMEBUFFER, gFramebuffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, gColorBuffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, gDepthBuffer);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    glBindRenderbuffer(GL_RENDERBUFFER, gColorBuffer);
    fprintf(stderr, "[host] framebuffer de pantalla %dx%d (estado 0x%04X)\n", width, height, status);
    return status == GL_FRAMEBUFFER_COMPLETE;
}

bool rl_host_gl_create(void *layer) {
    gLayer = (__bridge CAEAGLLayer *)layer;
    gContext = [[EAGLContext alloc] initWithAPI:kEAGLRenderingAPIOpenGLES3];
    if (gContext == nil) {
        fprintf(stderr, "[host] este dispositivo no ofrece OpenGL ES 3\n");
        return false;
    }
    if (![EAGLContext setCurrentContext:gContext]) {
        fprintf(stderr, "[host] no pude activar el contexto OpenGL ES\n");
        return false;
    }
    glGenFramebuffers(1, &gFramebuffer);
    glGenRenderbuffers(1, &gColorBuffer);
    glGenRenderbuffers(1, &gDepthBuffer);
    bool ok = allocate_storage();
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    return ok;
}

// Called at the start of a frame after a size change. The engine's own bindings are put back, so
// the frame goes on drawing wherever it was drawing.
bool rl_host_gl_update_drawable(void) {
    if (gContext == nil) return false;
    GLint draw = 0, read = 0, renderbuffer = 0;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read);
    glGetIntegerv(GL_RENDERBUFFER_BINDING, &renderbuffer);
    bool ok = allocate_storage();
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, (GLuint)draw);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)read);
    glBindRenderbuffer(GL_RENDERBUFFER, (GLuint)renderbuffer);
    return ok;
}

uint32_t rl_host_gl_framebuffer(void) {
    return gFramebuffer;
}

bool rl_host_gl_present(void) {
    if (gContext == nil) return false;
    GLint renderbuffer = 0;
    glGetIntegerv(GL_RENDERBUFFER_BINDING, &renderbuffer);
    glBindRenderbuffer(GL_RENDERBUFFER, gColorBuffer);
    BOOL ok = [gContext presentRenderbuffer:GL_RENDERBUFFER];
    glBindRenderbuffer(GL_RENDERBUFFER, (GLuint)renderbuffer);
    return ok;
}

void rl_host_gl_destroy(void) {
    if (gContext == nil) return;
    glDeleteFramebuffers(1, &gFramebuffer);
    glDeleteRenderbuffers(1, &gColorBuffer);
    glDeleteRenderbuffers(1, &gDepthBuffer);
    gFramebuffer = gColorBuffer = gDepthBuffer = 0;
    [EAGLContext setCurrentContext:nil];
    gContext = nil;
}

// -------------------------------------------------------------------------------------------------
// The game's view: a CAEAGLLayer that takes touches and typed text.

@interface GameView : UIView <UIKeyInput>
@end

@implementation GameView {
    NSMutableDictionary<NSValue *, NSNumber *> *_touchIds;
    int32_t _nextTouchId;
}

+ (Class)layerClass {
    return [CAEAGLLayer class];
}

- (instancetype)initWithFrame:(CGRect)frame {
    if ((self = [super initWithFrame:frame])) {
        CAEAGLLayer *layer = (CAEAGLLayer *)self.layer;
        layer.opaque = YES;
        layer.drawableProperties = @{
            kEAGLDrawablePropertyRetainedBacking : @NO,
            kEAGLDrawablePropertyColorFormat : kEAGLColorFormatRGBA8,
        };
        self.contentScaleFactor = UIScreen.mainScreen.nativeScale;
        self.multipleTouchEnabled = YES;
        self.backgroundColor = UIColor.blackColor;
        _touchIds = [NSMutableDictionary new];
    }
    return self;
}

- (void)didMoveToWindow {
    [super didMoveToWindow];
    if (self.window.screen != nil) {
        self.contentScaleFactor = self.window.screen.nativeScale;
    }
}

// Touches travel in pixels of this view, the unit the engine's window uses. Each finger keeps a
// small number for its whole gesture.
- (void)forwardTouches:(NSSet<UITouch *> *)touches phase:(int32_t)phase {
    CGFloat scale = self.contentScaleFactor;
    for (UITouch *touch in touches) {
        NSValue *key = [NSValue valueWithPointer:(__bridge const void *)touch];
        NSNumber *number = _touchIds[key];
        if (number == nil) {
            if (phase != 0) continue;
            _nextTouchId = (_nextTouchId + 1) & 0x7fffffff;
            number = @(_nextTouchId);
            _touchIds[key] = number;
        }
        CGPoint point = [touch locationInView:self];
        rl_ios_touch(phase, number.intValue, (float)(point.x * scale), (float)(point.y * scale));
        if (phase >= 2) [_touchIds removeObjectForKey:key];
    }
}

- (void)touchesBegan:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
    [self forwardTouches:touches phase:0];
}

- (void)touchesMoved:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
    [self forwardTouches:touches phase:1];
}

- (void)touchesEnded:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
    [self forwardTouches:touches phase:2];
}

- (void)touchesCancelled:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
    // The engine lets go of everything on a cancel; once is enough.
    rl_ios_touch(3, 0, 0, 0);
    [_touchIds removeAllObjects];
}

// UIKeyInput: the system keyboard types into the game's text dialog.
- (BOOL)canBecomeFirstResponder {
    return YES;
}

- (BOOL)hasText {
    // Always "has text", so the delete key keeps working on what the game holds.
    return YES;
}

- (void)insertText:(NSString *)text {
    if ([text isEqualToString:@"\n"]) {
        rl_ios_enter();
    } else {
        rl_ios_commit_text(text.UTF8String);
    }
}

- (void)deleteBackward {
    rl_ios_backspace();
}

- (UITextAutocorrectionType)autocorrectionType {
    return UITextAutocorrectionTypeNo;
}

- (UITextAutocapitalizationType)autocapitalizationType {
    return UITextAutocapitalizationTypeNone;
}

- (UITextSpellCheckingType)spellCheckingType {
    return UITextSpellCheckingTypeNo;
}

- (UITextSmartQuotesType)smartQuotesType {
    return UITextSmartQuotesTypeNo;
}

- (UITextSmartDashesType)smartDashesType {
    return UITextSmartDashesTypeNo;
}

- (UIReturnKeyType)returnKeyType {
    return UIReturnKeyDone;
}

- (UIKeyboardAppearance)keyboardAppearance {
    return UIKeyboardAppearanceDark;
}

@end

// -------------------------------------------------------------------------------------------------
// The only screen.

@interface GameViewController : UIViewController
- (void)showMessage:(NSString *)text;
@end

@implementation GameViewController {
    GameView *_gameView;
    UILabel *_message;
    CADisplayLink *_displayLink;
    BOOL _started;
    uint32_t _width, _height;
}

- (void)loadView {
    UIView *root = [UIView new];
    root.backgroundColor = UIColor.blackColor;
    self.view = root;
}

- (void)viewDidLoad {
    [super viewDidLoad];
    _gameView = [[GameView alloc] initWithFrame:self.view.bounds];
    [self.view addSubview:_gameView];
    gGameView = _gameView;

    _message = [UILabel new];
    _message.numberOfLines = 0;
    _message.textAlignment = NSTextAlignmentCenter;
    _message.textColor = UIColor.whiteColor;
    _message.font = [UIFont systemFontOfSize:17];
    _message.backgroundColor = [UIColor colorWithWhite:0 alpha:0.85];
    _message.hidden = YES;
    [self.view addSubview:_message];

    NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
    [center addObserver:self
               selector:@selector(keyboardWillChangeFrame:)
                   name:UIKeyboardWillChangeFrameNotification
                 object:nil];
    [center addObserver:self selector:@selector(keyboardWillHide:) name:UIKeyboardWillHideNotification object:nil];
}

- (void)viewDidLayoutSubviews {
    [super viewDidLayoutSubviews];
    // The game stays clear of the notch and the rounded corners on the sides; the controls near the
    // edges would otherwise sit under them.
    UIEdgeInsets safe = self.view.safeAreaInsets;
    CGRect frame = self.view.bounds;
    frame.origin.x += safe.left;
    frame.size.width -= safe.left + safe.right;
    _gameView.frame = frame;
    _message.frame = CGRectInset(frame, 24, 24);

    CGFloat scale = _gameView.contentScaleFactor;
    uint32_t width = (uint32_t)lround(frame.size.width * scale);
    uint32_t height = (uint32_t)lround(frame.size.height * scale);
    if (width == 0 || height == 0) return;
    if (!_started) {
        // The game lays itself out from the first size it gets, so wait for the landscape one.
        if (width <= height) return;
        _started = YES;
        _width = width;
        _height = height;
        NSString *documents =
            NSSearchPathForDirectoriesInDomains(NSDocumentDirectory, NSUserDomainMask, YES).firstObject;
        NSString *bundle = NSBundle.mainBundle.resourcePath;
        rl_ios_start(documents.fileSystemRepresentation, bundle.fileSystemRepresentation,
                     (__bridge void *)_gameView.layer, width, height);
        _displayLink = [CADisplayLink displayLinkWithTarget:self selector:@selector(tick)];
        [_displayLink addToRunLoop:NSRunLoop.mainRunLoop forMode:NSRunLoopCommonModes];
    } else if (width != _width || height != _height) {
        _width = width;
        _height = height;
        rl_ios_resize(width, height);
    }
}

- (void)tick {
    rl_ios_tick();
}

- (void)showMessage:(NSString *)text {
    _message.text = text.length > 0 ? text : nil;
    _message.hidden = text.length == 0;
}

// How much of the game view the keyboard covers, in pixels, so the engine keeps the game above it.
- (void)keyboardWillChangeFrame:(NSNotification *)note {
    CGRect end = [note.userInfo[UIKeyboardFrameEndUserInfoKey] CGRectValue];
    CGRect keyboard = [_gameView convertRect:end fromView:nil];
    CGRect covered = CGRectIntersection(keyboard, _gameView.bounds);
    CGFloat height = CGRectIsNull(covered) ? 0 : covered.size.height;
    rl_ios_ime_inset((uint32_t)lround(height * _gameView.contentScaleFactor));
}

- (void)keyboardWillHide:(NSNotification *)note {
    rl_ios_ime_inset(0);
}

- (BOOL)prefersStatusBarHidden {
    return YES;
}

- (BOOL)prefersHomeIndicatorAutoHidden {
    return YES;
}

// A swipe from an edge is far more likely to be the movement stick than a request for the
// Control Center; the system asks for a second swipe instead.
- (UIRectEdge)preferredScreenEdgesDeferringSystemGestures {
    return UIRectEdgeAll;
}

- (UIInterfaceOrientationMask)supportedInterfaceOrientations {
    return UIInterfaceOrientationMaskLandscape;
}

@end

// -------------------------------------------------------------------------------------------------
// Callbacks from the engine. They can arrive on any thread; UIKit is only touched on the main one.

void rl_host_set_keyboard(bool visible) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (visible) {
            [gGameView becomeFirstResponder];
        } else {
            [gGameView resignFirstResponder];
        }
    });
}

void rl_host_haptic(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        static UIImpactFeedbackGenerator *generator;
        if (generator == nil) {
            generator = [[UIImpactFeedbackGenerator alloc] initWithStyle:UIImpactFeedbackStyleLight];
        }
        [generator impactOccurred];
    });
}

void rl_host_show_message(const char *text) {
    NSString *message = text != NULL ? [NSString stringWithUTF8String:text] : @"";
    if (message == nil) message = @"";
    dispatch_async(dispatch_get_main_queue(), ^{
        [gController showMessage:message];
    });
}

void rl_host_game_ended(int code) {
    fprintf(stderr, "[host] el juego termino (%d); se cierra la app\n", code);
    dispatch_async(dispatch_get_main_queue(), ^{
        exit(0);
    });
}

// -------------------------------------------------------------------------------------------------
// App and scene lifecycle.

@interface SceneDelegate : UIResponder <UIWindowSceneDelegate>
@property(strong, nonatomic) UIWindow *window;
@end

@implementation SceneDelegate

- (void)scene:(UIScene *)scene
    willConnectToSession:(UISceneSession *)session
                 options:(UISceneConnectionOptions *)connectionOptions {
    UIWindowScene *windowScene = (UIWindowScene *)scene;
    self.window = [[UIWindow alloc] initWithWindowScene:windowScene];
    gController = [GameViewController new];
    self.window.rootViewController = gController;
    [self.window makeKeyAndVisible];
}

- (void)sceneDidBecomeActive:(UIScene *)scene {
    rl_ios_set_active(true);
}

// Returns only once the engine has stopped drawing: iOS kills apps that use OpenGL ES in the
// background.
- (void)sceneWillResignActive:(UIScene *)scene {
    rl_ios_set_active(false);
}

@end

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    // Ambient: the game follows the silent switch and does not stop music from other apps.
    AVAudioSession *session = AVAudioSession.sharedInstance;
    [session setCategory:AVAudioSessionCategoryAmbient error:nil];
    [session setActive:YES error:nil];
    [NSNotificationCenter.defaultCenter addObserverForName:AVAudioSessionInterruptionNotification
                                                    object:session
                                                     queue:NSOperationQueue.mainQueue
                                                usingBlock:^(NSNotification *note) {
        NSUInteger type = [note.userInfo[AVAudioSessionInterruptionTypeKey] unsignedIntegerValue];
        if (type == AVAudioSessionInterruptionTypeBegan) {
            rl_ios_audio_interrupted(true);
        } else {
            [AVAudioSession.sharedInstance setActive:YES error:nil];
            rl_ios_audio_interrupted(false);
        }
    }];
    // A game played with the thumbs would otherwise dim and lock the screen mid-battle.
    application.idleTimerDisabled = YES;
    return YES;
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil, NSStringFromClass([AppDelegate class]));
    }
}
