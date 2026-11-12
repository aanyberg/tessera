#import "ExceptionCatching.h"

NSException *_Nullable TesseraTestsCatchingException(NS_NOESCAPE void (^block)(void)) {
    @try {
        block();
    } @catch (NSException *exception) {
        return exception;
    }
    return nil;
}
