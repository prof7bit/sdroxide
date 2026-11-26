/* Stand-in for FreeDV's logger, for the vendored rade_text.cpp: its messages
   are dropped. The Rust side logs what is decoded. */
#ifndef SDROXIDE_ULOG_STUB_H
#define SDROXIDE_ULOG_STUB_H

#define log_debug(...) ((void)0)
#define log_info(...) ((void)0)

#endif
