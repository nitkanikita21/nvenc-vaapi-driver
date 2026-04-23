/* Backend-side VAAPI headers. We implement the VADriverVTable, we do not
 * link libva. Only type/struct/enum definitions are needed. */
#include <va/va_backend.h>
#include <va/va.h>
#include <va/va_enc_h264.h>
#include <va/va_drmcommon.h>
