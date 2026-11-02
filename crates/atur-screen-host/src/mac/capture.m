// ScreenCaptureKit and the pasteboard, behind a plain C interface for Rust.
//
// Everything here is called from threads that are not the main thread; the main
// thread runs the application's run loop (atur_app_run), which ScreenCaptureKit
// needs in order to reach the window server.

#import <AppKit/AppKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include <stdlib.h>
#include <string.h>

typedef void (*AturFrameFn)(void *ctx, CVPixelBufferRef frame);
typedef void (*AturStopFn)(void *ctx, int code, const char *message);

// SCStreamErrorUserDeclined: Screen Recording is not allowed for this app.
#define ATUR_USER_DECLINED (-3801)
#define ATUR_OK 0
#define ATUR_PERMISSION 1
#define ATUR_FAILED 2
#define ATUR_START_TIMEOUT_SECONDS 15

@interface AturCapture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, strong) SCStream *stream;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic) AturFrameFn onFrame;
@property(nonatomic) AturStopFn onStop;
@property(nonatomic) void *ctx;
@property(atomic) BOOL alive;
@end

static int atur_classify(NSError *error) {
    if (error != nil && error.code == ATUR_USER_DECLINED) {
        return ATUR_PERMISSION;
    }
    return ATUR_FAILED;
}

static void atur_report(NSError *error, const char *fallback, char *buffer, size_t length,
                        int *code) {
    const char *text = error != nil ? error.localizedDescription.UTF8String : NULL;
    if (text == NULL || text[0] == '\0') {
        text = fallback;
    }
    if (buffer != NULL && length > 0) {
        strlcpy(buffer, text, length);
    }
    if (code != NULL) {
        *code = atur_classify(error);
    }
}

@implementation AturCapture

- (void)stream:(SCStream *)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
    if (type != SCStreamOutputTypeScreen || !self.alive || !CMSampleBufferIsValid(sampleBuffer)) {
        return;
    }
    // Only pictures that carry new content: an idle frame has no image and a
    // started or stopped frame is not one.
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (attachments != NULL && CFArrayGetCount(attachments) > 0) {
        NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0);
        NSNumber *status = info[SCStreamFrameInfoStatus];
        if (status != nil && status.integerValue != SCFrameStatusComplete) {
            return;
        }
    }
    CVImageBufferRef image = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (image != NULL) {
        self.onFrame(self.ctx, image);
    }
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    if (!self.alive) {
        return;
    }
    self.alive = NO;
    const char *text = error.localizedDescription.UTF8String;
    self.onStop(self.ctx, atur_classify(error), text != NULL ? text : "The screen capture stopped.");
}

@end

// Held for the life of the process: while the screen is shared the program is
// doing work the user is waiting for, so the system must not nap it or sleep.
static id atur_activity;

void atur_app_init(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        atur_activity = [[NSProcessInfo processInfo]
            beginActivityWithOptions:(NSActivityUserInitiated | NSActivityLatencyCritical)
                              reason:@"Sharing this Mac's screen"];
    }
}

void atur_app_run(void) {
    [NSApp run];
}

// Starts capturing display `display_id` (the first display when it is not found)
// at width x height pixels, `fps` pictures a second at most, with the cursor.
// Returns a handle, or NULL with a message in `error` and a code in `code`
// (1: Screen Recording is not allowed, 2: anything else).
void *atur_capture_start(uint32_t display_id, int width, int height, int fps, AturFrameFn on_frame,
                         AturStopFn on_stop, void *ctx, char *error, size_t error_length,
                         int *code) {
    @autoreleasepool {
        dispatch_semaphore_t listed = dispatch_semaphore_create(0);
        __block SCShareableContent *content = nil;
        __block NSError *list_error = nil;
        [SCShareableContent
            getShareableContentWithCompletionHandler:^(SCShareableContent *found, NSError *failure) {
                content = found;
                list_error = failure;
                dispatch_semaphore_signal(listed);
            }];
        if (dispatch_semaphore_wait(listed, dispatch_time(DISPATCH_TIME_NOW, ATUR_START_TIMEOUT_SECONDS * NSEC_PER_SEC)) != 0) {
            atur_report(nil, "macOS did not answer when asked which displays can be shared.", error, error_length, code);
            return NULL;
        }
        if (content == nil) {
            atur_report(list_error, "macOS could not list the displays.", error, error_length, code);
            return NULL;
        }
        SCDisplay *display = nil;
        for (SCDisplay *candidate in content.displays) {
            if (candidate.displayID == display_id) {
                display = candidate;
                break;
            }
        }
        if (display == nil) {
            display = content.displays.firstObject;
        }
        if (display == nil) {
            atur_report(nil, "This Mac has no display to share.", error, error_length, code);
            return NULL;
        }

        SCStreamConfiguration *configuration = [[SCStreamConfiguration alloc] init];
        configuration.width = (size_t)width;
        configuration.height = (size_t)height;
        configuration.scalesToFit = YES;
        configuration.minimumFrameInterval = CMTimeMake(1, fps > 0 ? fps : 60);
        configuration.pixelFormat = kCVPixelFormatType_32BGRA;
        configuration.showsCursor = YES;
        configuration.queueDepth = 3;
        // The viewers and the encoder assume sRGB.
        configuration.colorSpaceName = kCGColorSpaceSRGB;

        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        AturCapture *capture = [[AturCapture alloc] init];
        capture.onFrame = on_frame;
        capture.onStop = on_stop;
        capture.ctx = ctx;
        capture.alive = YES;
        dispatch_queue_attr_t attributes = dispatch_queue_attr_make_with_qos_class(
            DISPATCH_QUEUE_SERIAL, QOS_CLASS_USER_INTERACTIVE, 0);
        capture.queue = dispatch_queue_create("atur.screen.capture", attributes);
        capture.stream = [[SCStream alloc] initWithFilter:filter
                                            configuration:configuration
                                                 delegate:capture];
        NSError *output_error = nil;
        if (![capture.stream addStreamOutput:capture
                                        type:SCStreamOutputTypeScreen
                          sampleHandlerQueue:capture.queue
                                       error:&output_error]) {
            atur_report(output_error, "The screen capture could not be set up.", error, error_length, code);
            return NULL;
        }

        dispatch_semaphore_t started = dispatch_semaphore_create(0);
        __block NSError *start_error = nil;
        [capture.stream startCaptureWithCompletionHandler:^(NSError *failure) {
            start_error = failure;
            dispatch_semaphore_signal(started);
        }];
        if (dispatch_semaphore_wait(started, dispatch_time(DISPATCH_TIME_NOW, ATUR_START_TIMEOUT_SECONDS * NSEC_PER_SEC)) != 0) {
            capture.alive = NO;
            atur_report(nil, "The screen capture did not start in time.", error, error_length, code);
            return NULL;
        }
        if (start_error != nil) {
            capture.alive = NO;
            atur_report(start_error, "The screen capture did not start.", error, error_length, code);
            return NULL;
        }
        if (code != NULL) {
            *code = ATUR_OK;
        }
        return (__bridge_retained void *)capture;
    }
}

// Stops the capture. When this returns no callback is running or will run.
void atur_capture_stop(void *handle) {
    if (handle == NULL) {
        return;
    }
    @autoreleasepool {
        AturCapture *capture = (__bridge_transfer AturCapture *)handle;
        capture.alive = NO;
        dispatch_semaphore_t stopped = dispatch_semaphore_create(0);
        [capture.stream stopCaptureWithCompletionHandler:^(__unused NSError *failure) {
            dispatch_semaphore_signal(stopped);
        }];
        dispatch_semaphore_wait(stopped, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
        // Anything already queued has run once this empty block does.
        dispatch_sync(capture.queue, ^{
                      });
    }
}

long atur_pasteboard_change_count(void) {
    return (long)[NSPasteboard generalPasteboard].changeCount;
}

// The clipboard's text as a malloc'ed UTF-8 string (free with atur_free), or NULL.
char *atur_pasteboard_copy_text(void) {
    @autoreleasepool {
        NSString *text = [[NSPasteboard generalPasteboard] stringForType:NSPasteboardTypeString];
        if (text == nil || text.length == 0) {
            return NULL;
        }
        const char *utf8 = text.UTF8String;
        return utf8 != NULL ? strdup(utf8) : NULL;
    }
}

// Replaces the clipboard's content with `utf8` text; returns the new change count.
long atur_pasteboard_set_text(const char *utf8) {
    @autoreleasepool {
        NSPasteboard *board = [NSPasteboard generalPasteboard];
        NSString *text = utf8 != NULL ? [NSString stringWithUTF8String:utf8] : nil;
        if (text != nil) {
            [board clearContents];
            [board setString:text forType:NSPasteboardTypeString];
        }
        return (long)board.changeCount;
    }
}

void atur_free(void *pointer) {
    free(pointer);
}
