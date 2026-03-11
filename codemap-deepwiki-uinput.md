 Linux uinput / evdev query

  Research target: Linux kernel uinput semantics plus the Rust evdev crate path used by Parakeet.

  Goal:
  Determine whether Parakeet’s current uinput implementation is likely to suffer from fresh-device first-event races, and what
  the correct persistent-plus-retryable design should be.

  Please trace from Parakeet code:
  - parakeet-ptt/src/injector.rs
    - UinputChordSender::new
    - UinputChordSender::send_shortcut
  - the evdev crate source for:
    - uinput::VirtualDeviceBuilder::new
    - VirtualDeviceBuilder::build
    - VirtualDevice::emit
  - Linux kernel uinput documentation and, if needed, kernel implementation references

  Answer these specific questions with code/doc citations:
  1. Does VirtualDeviceBuilder::build create a brand new kernel virtual device every time Parakeet rebuilds the injector?
  2. After UI_DEV_CREATE / build(), what guarantees exist before the first emitted key event is seen by userspace consumers
  like libinput/compositors?
  3. The kernel docs mention sleeping after device creation so userspace can detect the new device. What exact problem is that
  guarding against, and is it plausibly relevant to a Wayland compositor receiving Ctrl+Shift+V?
  4. Does VirtualDevice::emit flush SYN_REPORT automatically per call, and what exact event packet sequence does Parakeet’s
  current send_shortcut produce for Ctrl+V and Ctrl+Shift+V?
  5. Would keeping one VirtualDevice open across many injections change kernel/compositor semantics in a way that could
  realistically improve reliability, or would it only save setup overhead?
  6. If device creation fails at startup but later becomes possible, what is the cleanest retry model:
     - lazy init on first send
     - retry on every send after failure
     - background re-init with backoff
     - persistent handle with re-open on error
  7. What observable conditions can Parakeet check to distinguish:
     - /dev/uinput unavailable
     - virtual device created but not yet noticed by compositor/userspace
     - events emitted successfully but shortcut not accepted by target app
  8. Based on ground truth, what should Parakeet do next if the product goal is “paste works every time”:
     - persistent device while healthy
     - retryable re-init on failure
     - explicit warm-up wait after device creation
     - never rebuild per job
     - some combination of the above

  Deliverable:
  - exact ioctl/build/emit lifecycle
  - first-event race analysis
  - recommended state machine for a persistent-but-recoverable uinput sender

- 
## Linux Kernel uinput Subsystem Analysis
This codemap traces the Linux kernel's uinput subsystem implementation, focusing on device creation races, event synchronization protocols, and the critical timing requirements between device creation and first event delivery. Key insights: [1b] shows explicit sleep requirement for race avoidance, [2b-2d] demonstrates mandatory SYN_REPORT sequencing, and [3b] reveals dynamic device node creation behavior.
### 1. uinput Device Creation Lifecycle
Traces the complete lifecycle of virtual device creation from userspace ioctl to kernel device registration
### 1a. Userspace Device Creation (`uinput.rst:83`)
Userspace initiates virtual device creation via UI_DEV_CREATE ioctl
```text
ioctl(fd, UI_DEV_CREATE);
```
### 1b. Critical Race Guard (`uinput.rst:92`)
Documentation explicitly recommends sleeping to allow userspace detection
```text
sleep(1);
```
### 1c. First Event Emission (`uinput.rst:95`)
First key event sent after device creation and sleep
```text
emit(fd, EV_KEY, KEY_SPACE, 1);
```
### 2. Event Synchronization Protocol
Shows the required SYN_REPORT sequence for proper event delivery to userspace consumers
### 2a. Key Press Event (`uinput.rst:95`)
Key press event with value 1
```text
emit(fd, EV_KEY, KEY_SPACE, 1);
```
### 2b. Synchronization Report (`uinput.rst:96`)
Required SYN_REPORT to flush the event packet
```text
emit(fd, EV_SYN, SYN_REPORT, 0);
```
### 2c. Key Release Event (`uinput.rst:97`)
Key release event with value 0
```text
emit(fd, EV_KEY, KEY_SPACE, 0);
```
### 2d. Final Sync (`uinput.rst:98`)
Second SYN_REPORT to complete the key sequence
```text
emit(fd, EV_SYN, SYN_REPORT, 0);
```
### 3. Device Node Registration
Shows how uinput creates the /dev/input/eventX device node that userspace discovers
### 3a. Device Node Assignment (`devices.txt:359`)
uinput control device assigned major/minor 223
```text
223 = /dev/input/uinput	User level driver support for input
```
### 3b. Dynamic Node Creation (`uinput.rst:86`)
Kernel creates /dev/input/eventX nodes on device creation
```text
On UI_DEV_CREATE the kernel will create the device node for this device
```
### 4. Input Event Structure
Defines the kernel input event format that flows from uinput to userspace consumers
### 4a. Event Structure Definition (`input.h:28`)
Core input event structure with timestamp, type, code, value
```text
struct input_event {
```
### 4b. Event Payload (`input.h:44`)
Event type (EV_KEY/EV_SYN), code (KEY_*), and value (0/1)
```text
__u16 type; __u16 code; __s32 value;
```
### 5. ioctl Interface Definition
Shows the ioctl number allocation and interface for uinput control operations
### 5a. ioctl Number Range (`ioctl-number.rst:209`)
uinput ioctl codes allocated in 'U' range 00-CF
```text
'U'   00-CF  linux/uinput.h                                            conflict!
```
# DeepWiki Q&A with Code Context for Repository: torvalds/linux
## Q1
Linux uinput / evdev query

  Research target: Linux kernel uinput semantics plus the Rust evdev crate path used by Parakeet.

  Goal:
  Determine whether Parakeet’s current uinput implementation is likely to suffer from fresh-device first-event races, and what
  the correct persistent-plus-retryable design should be.

  Please trace from Parakeet code:
  - parakeet-ptt/src/injector.rs
    - UinputChordSender::new
    - UinputChordSender::send_shortcut
  - the evdev crate source for:
    - uinput::VirtualDeviceBuilder::new
    - VirtualDeviceBuilder::build
    - VirtualDevice::emit
  - Linux kernel uinput documentation and, if needed, kernel implementation references

  Answer these specific questions with code/doc citations:
  1. Does VirtualDeviceBuilder::build create a brand new kernel virtual device every time Parakeet rebuilds the injector?
  2. After UI_DEV_CREATE / build(), what guarantees exist before the first emitted key event is seen by userspace consumers
  like libinput/compositors?
  3. The kernel docs mention sleeping after device creation so userspace can detect the new device. What exact problem is that
  guarding against, and is it plausibly relevant to a Wayland compositor receiving Ctrl+Shift+V?
  4. Does VirtualDevice::emit flush SYN_REPORT automatically per call, and what exact event packet sequence does Parakeet’s
  current send_shortcut produce for Ctrl+V and Ctrl+Shift+V?
  5. Would keeping one VirtualDevice open across many injections change kernel/compositor semantics in a way that could
  realistically improve reliability, or would it only save setup overhead?
  6. If device creation fails at startup but later becomes possible, what is the cleanest retry model:
     - lazy init on first send
     - retry on every send after failure
     - background re-init with backoff
     - persistent handle with re-open on error
  7. What observable conditions can Parakeet check to distinguish:
     - /dev/uinput unavailable
     - virtual device created but not yet noticed by compositor/userspace
     - events emitted successfully but shortcut not accepted by target app
  8. Based on ground truth, what should Parakeet do next if the product goal is “paste works every time”:
     - persistent device while healthy
     - retryable re-init on failure
     - explicit warm-up wait after device creation
     - never rebuild per job
     - some combination of the above

  Deliverable:
  - exact ioctl/build/emit lifecycle
  - first-event race analysis
  - recommended state machine for a persistent-but-recoverable uinput sender
## Linux Kernel uinput Subsystem Analysis
This codemap traces the Linux kernel's uinput subsystem implementation, focusing on device creation races, event synchronization protocols, and the critical timing requirements between device creation and first event delivery. Key insights: [1b] shows explicit sleep requirement for race avoidance, [2b-2d] demonstrates mandatory SYN_REPORT sequencing, and [3b] reveals dynamic device node creation behavior.
### 1. uinput Device Creation Lifecycle
Traces the complete lifecycle of virtual device creation from userspace ioctl to kernel device registration
### 1a. Userspace Device Creation (`uinput.rst:83`)
Userspace initiates virtual device creation via UI_DEV_CREATE ioctl
```text
ioctl(fd, UI_DEV_CREATE);
```
### 1b. Critical Race Guard (`uinput.rst:92`)
Documentation explicitly recommends sleeping to allow userspace detection
```text
sleep(1);
```
### 1c. First Event Emission (`uinput.rst:95`)
First key event sent after device creation and sleep
```text
emit(fd, EV_KEY, KEY_SPACE, 1);
```
### 2. Event Synchronization Protocol
Shows the required SYN_REPORT sequence for proper event delivery to userspace consumers
### 2a. Key Press Event (`uinput.rst:95`)
Key press event with value 1
```text
emit(fd, EV_KEY, KEY_SPACE, 1);
```
### 2b. Synchronization Report (`uinput.rst:96`)
Required SYN_REPORT to flush the event packet
```text
emit(fd, EV_SYN, SYN_REPORT, 0);
```
### 2c. Key Release Event (`uinput.rst:97`)
Key release event with value 0
```text
emit(fd, EV_KEY, KEY_SPACE, 0);
```
### 2d. Final Sync (`uinput.rst:98`)
Second SYN_REPORT to complete the key sequence
```text
emit(fd, EV_SYN, SYN_REPORT, 0);
```
### 3. Device Node Registration
Shows how uinput creates the /dev/input/eventX device node that userspace discovers
### 3a. Device Node Assignment (`devices.txt:359`)
uinput control device assigned major/minor 223
```text
223 = /dev/input/uinput	User level driver support for input
```
### 3b. Dynamic Node Creation (`uinput.rst:86`)
Kernel creates /dev/input/eventX nodes on device creation
```text
On UI_DEV_CREATE the kernel will create the device node for this device
```
### 4. Input Event Structure
Defines the kernel input event format that flows from uinput to userspace consumers
### 4a. Event Structure Definition (`input.h:28`)
Core input event structure with timestamp, type, code, value
```text
struct input_event {
```
### 4b. Event Payload (`input.h:44`)
Event type (EV_KEY/EV_SYN), code (KEY_*), and value (0/1)
```text
__u16 type; __u16 code; __s32 value;
```
### 5. ioctl Interface Definition
Shows the ioctl number allocation and interface for uinput control operations
### 5a. ioctl Number Range (`ioctl-number.rst:209`)
uinput ioctl codes allocated in 'U' range 00-CF
```text
'U'   00-CF  linux/uinput.h                                            conflict!
```
---

=============
uinput module
=============

Introduction
============

uinput is a kernel module that makes it possible to emulate input devices
from userspace. By writing to /dev/uinput (or /dev/input/uinput) device, a
process can create a virtual input device with specific capabilities. Once
this virtual device is created, the process can send events through it,
that will be delivered to userspace and in-kernel consumers.

Interface
=========

::

  linux/uinput.h

The uinput header defines ioctls to create, set up, and destroy virtual
devices.

libevdev
========

libevdev is a wrapper library for evdev devices that provides interfaces to
create uinput devices and send events. libevdev is less error-prone than
accessing uinput directly, and should be considered for new software.

For examples and more information about libevdev:
https://www.freedesktop.org/software/libevdev/doc/latest/

Examples
========

Keyboard events
---------------

This first example shows how to create a new virtual device, and how to
send a key event. All default imports and error handlers were removed for
the sake of simplicity.

.. code-block:: c

   #include <linux/uinput.h>

   void emit(int fd, int type, int code, int val)
   {
      struct input_event ie;

      ie.type = type;
      ie.code = code;
      ie.value = val;
      /* timestamp values below are ignored */
      ie.time.tv_sec = 0;
      ie.time.tv_usec = 0;

      write(fd, &ie, sizeof(ie));
   }

   int main(void)
   {
      struct uinput_setup usetup;

      int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);


      /*
       * The ioctls below will enable the device that is about to be
       * created, to pass key events, in this case the space key.
       */
      ioctl(fd, UI_SET_EVBIT, EV_KEY);
      ioctl(fd, UI_SET_KEYBIT, KEY_SPACE);

      memset(&usetup, 0, sizeof(usetup));
      usetup.id.bustype = BUS_USB;
      usetup.id.vendor = 0x1234; /* sample vendor */
      usetup.id.product = 0x5678; /* sample product */
      strcpy(usetup.name, "Example device");

      ioctl(fd, UI_DEV_SETUP, &usetup);
      ioctl(fd, UI_DEV_CREATE);

      /*
       * On UI_DEV_CREATE the kernel will create the device node for this
       * device. We are inserting a pause here so that userspace has time
       * to detect, initialize the new device, and can start listening to
       * the event, otherwise it will not notice the event we are about
       * to send. This pause is only needed in our example code!
       */
      sleep(1);

      /* Key press, report the event, send key release, and report again */
      emit(fd, EV_KEY, KEY_SPACE, 1);
      emit(fd, EV_SYN, SYN_REPORT, 0);
      emit(fd, EV_KEY, KEY_SPACE, 0);
      emit(fd, EV_SYN, SYN_REPORT, 0);

      /*
       * Give userspace some time to read the events before we destroy the
       * device with UI_DEV_DESTROY.
       */
      sleep(1);

      ioctl(fd, UI_DEV_DESTROY);
      close(fd);

      return 0;
   }

Mouse movements
---------------

This example shows how to create a virtual device that behaves like a physical
mouse.

.. code-block:: c

   #include <linux/uinput.h>

   /* emit function is identical to of the first example */

   int main(void)
   {
      struct uinput_setup usetup;
      int i = 50;

      int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);

      /* enable mouse button left and relative events */
      ioctl(fd, UI_SET_EVBIT, EV_KEY);
      ioctl(fd, UI_SET_KEYBIT, BTN_LEFT);

      ioctl(fd, UI_SET_EVBIT, EV_REL);
      ioctl(fd, UI_SET_RELBIT, REL_X);
      ioctl(fd, UI_SET_RELBIT, REL_Y);

      memset(&usetup, 0, sizeof(usetup));
      usetup.id.bustype = BUS_USB;
      usetup.id.vendor = 0x1234; /* sample vendor */
      usetup.id.product = 0x5678; /* sample product */
      strcpy(usetup.name, "Example device");

      ioctl(fd, UI_DEV_SETUP, &usetup);
      ioctl(fd, UI_DEV_CREATE);

      /*
       * On UI_DEV_CREATE the kernel will create the device node for this
       * device. We are inserting a pause here so that userspace has time
       * to detect, initialize the new device, and can start listening to
       * the event, otherwise it will not notice the event we are about
       * to send. This pause is only needed in our example code!
       */
      sleep(1);

      /* Move the mouse diagonally, 5 units per axis */
      while (i--) {
         emit(fd, EV_REL, REL_X, 5);
         emit(fd, EV_REL, REL_Y, 5);
         emit(fd, EV_SYN, SYN_REPORT, 0);
         usleep(15000);
      }

      /*
       * Give userspace some time to read the events before we destroy the
       * device with UI_DEV_DESTROY.
       */
      sleep(1);

      ioctl(fd, UI_DEV_DESTROY);
      close(fd);

      return 0;
   }


uinput old interface
--------------------

Before uinput version 5, there wasn't a dedicated ioctl to set up a virtual
device. Programs supporting older versions of uinput interface need to fill
a uinput_user_dev structure and write it to the uinput file descriptor to
configure the new uinput device. New code should not use the old interface
but interact with uinput via ioctl calls, or use libevdev.

.. code-block:: c

   #include <linux/uinput.h>

   /* emit function is identical to of the first example */

   int main(void)
   {
      struct uinput_user_dev uud;
      int version, rc, fd;

      fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
      rc = ioctl(fd, UI_GET_VERSION, &version);

      if (rc == 0 && version >= 5) {
         /* use UI_DEV_SETUP */
         return 0;
      }

      /*
       * The ioctls below will enable the device that is about to be
       * created, to pass key events, in this case the space key.
       */
      ioctl(fd, UI_SET_EVBIT, EV_KEY);
      ioctl(fd, UI_SET_KEYBIT, KEY_SPACE);

      memset(&uud, 0, sizeof(uud));
      snprintf(uud.name, UINPUT_MAX_NAME_SIZE, "uinput old interface");
      write(fd, &uud, sizeof(uud));

      ioctl(fd, UI_DEV_CREATE);

      /*
       * On UI_DEV_CREATE the kernel will create the device node for this
       * device. We are inserting a pause here so that userspace has time
       * to detect, initialize the new device, and can start listening to
       * the event, otherwise it will not notice the event we are about
       * to send. This pause is only needed in our example code!
       */
      sleep(1);

      /* Key press, report the event, send key release, and report again */
      emit(fd, EV_KEY, KEY_SPACE, 1);
      emit(fd, EV_SYN, SYN_REPORT, 0);
      emit(fd, EV_KEY, KEY_SPACE, 0);
      emit(fd, EV_SYN, SYN_REPORT, 0);

      /*
       * Give userspace some time to read the events before we destroy the
       * device with UI_DEV_DESTROY.
       */
      sleep(1);

      ioctl(fd, UI_DEV_DESTROY);

      close(fd);
      return 0;
   }
--
   0		Unnamed devices (e.g. non-device mounts)
		  0 = reserved as null device number
		See block major 144, 145, 146 for expansion areas.

   1 char	Memory devices
		  1 = /dev/mem		Physical memory access
		  2 = /dev/kmem		OBSOLETE - replaced by /proc/kcore
		  3 = /dev/null		Null device
		  4 = /dev/port		I/O port access
		  5 = /dev/zero		Null byte source
		  6 = /dev/core		OBSOLETE - replaced by /proc/kcore
		  7 = /dev/full		Returns ENOSPC on write
		  8 = /dev/random	Nondeterministic random number gen.
		  9 = /dev/urandom	Faster, less secure random number gen.
		 10 = /dev/aio		Asynchronous I/O notification interface
		 11 = /dev/kmsg		Writes to this come out as printk's, reads
					export the buffered printk records.
		 12 = /dev/oldmem	OBSOLETE - replaced by /proc/vmcore

   1 block	RAM disk
		  0 = /dev/ram0		First RAM disk
		  1 = /dev/ram1		Second RAM disk
		    ...
		250 = /dev/initrd	Initial RAM disk

		Older kernels had /dev/ramdisk (1, 1) here.
		/dev/initrd refers to a RAM disk which was preloaded
		by the boot loader; newer kernels use /dev/ram0 for
		the initrd.

   2 char	Pseudo-TTY masters
		  0 = /dev/ptyp0	First PTY master
		  1 = /dev/ptyp1	Second PTY master
		    ...
		255 = /dev/ptyef	256th PTY master

		Pseudo-tty's are named as follows:
		* Masters are "pty", slaves are "tty";
		* the fourth letter is one of pqrstuvwxyzabcde indicating
		  the 1st through 16th series of 16 pseudo-ttys each, and
		* the fifth letter is one of 0123456789abcdef indicating
		  the position within the series.

		These are the old-style (BSD) PTY devices; Unix98
		devices are on major 128 and above and use the PTY
		master multiplex (/dev/ptmx) to acquire a PTY on
		demand.

   2 block	Floppy disks
		  0 = /dev/fd0		Controller 0, drive 0, autodetect
		  1 = /dev/fd1		Controller 0, drive 1, autodetect
		  2 = /dev/fd2		Controller 0, drive 2, autodetect
		  3 = /dev/fd3		Controller 0, drive 3, autodetect
		128 = /dev/fd4		Controller 1, drive 0, autodetect
		129 = /dev/fd5		Controller 1, drive 1, autodetect
		130 = /dev/fd6		Controller 1, drive 2, autodetect
		131 = /dev/fd7		Controller 1, drive 3, autodetect

		To specify format, add to the autodetect device number:
		  0 = /dev/fd?		Autodetect format
		  4 = /dev/fd?d360	5.25"  360K in a 360K  drive(1)
		 20 = /dev/fd?h360	5.25"  360K in a 1200K drive(1)
		 48 = /dev/fd?h410	5.25"  410K in a 1200K drive
		 64 = /dev/fd?h420	5.25"  420K in a 1200K drive
		 24 = /dev/fd?h720	5.25"  720K in a 1200K drive
		 80 = /dev/fd?h880	5.25"  880K in a 1200K drive(1)
		  8 = /dev/fd?h1200	5.25" 1200K in a 1200K drive(1)
		 40 = /dev/fd?h1440	5.25" 1440K in a 1200K drive(1)
		 56 = /dev/fd?h1476	5.25" 1476K in a 1200K drive
		 72 = /dev/fd?h1494	5.25" 1494K in a 1200K drive
		 92 = /dev/fd?h1600	5.25" 1600K in a 1200K drive(1)

		 12 = /dev/fd?u360	3.5"   360K Double Density(2)
		 16 = /dev/fd?u720	3.5"   720K Double Density(1)
		120 = /dev/fd?u800	3.5"   800K Double Density(2)
		 52 = /dev/fd?u820	3.5"   820K Double Density
		 68 = /dev/fd?u830	3.5"   830K Double Density
		 84 = /dev/fd?u1040	3.5"  1040K Double Density(1)
		 88 = /dev/fd?u1120	3.5"  1120K Double Density(1)
		 28 = /dev/fd?u1440	3.5"  1440K High Density(1)
		124 = /dev/fd?u1600	3.5"  1600K High Density(1)
		 44 = /dev/fd?u1680	3.5"  1680K High Density(3)
		 60 = /dev/fd?u1722	3.5"  1722K High Density
		 76 = /dev/fd?u1743	3.5"  1743K High Density
		 96 = /dev/fd?u1760	3.5"  1760K High Density
		116 = /dev/fd?u1840	3.5"  1840K High Density(3)
		100 = /dev/fd?u1920	3.5"  1920K High Density(1)
		 32 = /dev/fd?u2880	3.5"  2880K Extra Density(1)
		104 = /dev/fd?u3200	3.5"  3200K Extra Density
		108 = /dev/fd?u3520	3.5"  3520K Extra Density
		112 = /dev/fd?u3840	3.5"  3840K Extra Density(1)

		 36 = /dev/fd?CompaQ	Compaq 2880K drive; obsolete?

		(1) Autodetectable format
		(2) Autodetectable format in a Double Density (720K) drive only
		(3) Autodetectable format in a High Density (1440K) drive only

		NOTE: The letter in the device name (d, q, h or u)
		signifies the type of drive: 5.25" Double Density (d),
		5.25" Quad Density (q), 5.25" High Density (h) or 3.5"
		(any model, u).	 The use of the capital letters D, H
		and E for the 3.5" models have been deprecated, since
		the drive type is insignificant for these devices.

   3 char	Pseudo-TTY slaves
		  0 = /dev/ttyp0	First PTY slave
		  1 = /dev/ttyp1	Second PTY slave
		    ...
		255 = /dev/ttyef	256th PTY slave

		These are the old-style (BSD) PTY devices; Unix98
		devices are on major 136 and above.

   3 block	First MFM, RLL and IDE hard disk/CD-ROM interface
		  0 = /dev/hda		Master: whole disk (or CD-ROM)
		 64 = /dev/hdb		Slave: whole disk (or CD-ROM)

		For partitions, add to the whole disk device number:
		  0 = /dev/hd?		Whole disk
		  1 = /dev/hd?1		First partition
		  2 = /dev/hd?2		Second partition
		    ...
		 63 = /dev/hd?63	63rd partition

		For Linux/i386, partitions 1-4 are the primary
		partitions, and 5 and above are logical partitions.
		Other versions of Linux use partitioning schemes
		appropriate to their respective architectures.

   4 char	TTY devices
		  0 = /dev/tty0		Current virtual console

		  1 = /dev/tty1		First virtual console
		    ...
		 63 = /dev/tty63	63rd virtual console
		 64 = /dev/ttyS0	First UART serial port
		    ...
		255 = /dev/ttyS191	192nd UART serial port

		UART serial ports refer to 8250/16450/16550 series devices.

		Older versions of the Linux kernel used this major
		number for BSD PTY devices.  As of Linux 2.1.115, this
		is no longer supported.	 Use major numbers 2 and 3.

   4 block	Aliases for dynamically allocated major devices to be used
		when its not possible to create the real device nodes
		because the root filesystem is mounted read-only.

		   0 = /dev/root

   5 char	Alternate TTY devices
		  0 = /dev/tty		Current TTY device
		  1 = /dev/console	System console
		  2 = /dev/ptmx		PTY master multiplex
		  3 = /dev/ttyprintk	User messages via printk TTY device
		 64 = /dev/cua0		Callout device for ttyS0
		    ...
		255 = /dev/cua191	Callout device for ttyS191

		(5,1) is /dev/console starting with Linux 2.1.71.  See
		the section on terminal devices for more information
		on /dev/console.

   6 char	Parallel printer devices
		  0 = /dev/lp0		Parallel printer on parport0
		  1 = /dev/lp1		Parallel printer on parport1
		    ...

		Current Linux kernels no longer have a fixed mapping
		between parallel ports and I/O addresses.  Instead,
		they are redirected through the parport multiplex layer.

   7 char	Virtual console capture devices
		  0 = /dev/vcs		Current vc text (glyph) contents
		  1 = /dev/vcs1		tty1 text (glyph) contents
		    ...
		 63 = /dev/vcs63	tty63 text (glyph) contents
		 64 = /dev/vcsu		Current vc text (unicode) contents
		65 = /dev/vcsu1		tty1 text (unicode) contents
		    ...
		127 = /dev/vcsu63	tty63 text (unicode) contents
		128 = /dev/vcsa		Current vc text/attribute (glyph) contents
		129 = /dev/vcsa1	tty1 text/attribute (glyph) contents
		    ...
		191 = /dev/vcsa63	tty63 text/attribute (glyph) contents

		NOTE: These devices permit both read and write access.

   7 block	Loopback devices
		  0 = /dev/loop0	First loop device
		  1 = /dev/loop1	Second loop device
		    ...

		The loop devices are used to mount filesystems not
		associated with block devices.	The binding to the
		loop devices is handled by mount(8) or losetup(8).

   8 block	SCSI disk devices (0-15)
		  0 = /dev/sda		First SCSI disk whole disk
		 16 = /dev/sdb		Second SCSI disk whole disk
		 32 = /dev/sdc		Third SCSI disk whole disk
		    ...
		240 = /dev/sdp		Sixteenth SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

   9 char	SCSI tape devices
		  0 = /dev/st0		First SCSI tape, mode 0
		  1 = /dev/st1		Second SCSI tape, mode 0
		    ...
		 32 = /dev/st0l		First SCSI tape, mode 1
		 33 = /dev/st1l		Second SCSI tape, mode 1
		    ...
		 64 = /dev/st0m		First SCSI tape, mode 2
		 65 = /dev/st1m		Second SCSI tape, mode 2
		    ...
		 96 = /dev/st0a		First SCSI tape, mode 3
		 97 = /dev/st1a		Second SCSI tape, mode 3
		      ...
		128 = /dev/nst0		First SCSI tape, mode 0, no rewind
		129 = /dev/nst1		Second SCSI tape, mode 0, no rewind
		    ...
		160 = /dev/nst0l	First SCSI tape, mode 1, no rewind
		161 = /dev/nst1l	Second SCSI tape, mode 1, no rewind
		    ...
		192 = /dev/nst0m	First SCSI tape, mode 2, no rewind
		193 = /dev/nst1m	Second SCSI tape, mode 2, no rewind
		    ...
		224 = /dev/nst0a	First SCSI tape, mode 3, no rewind
		225 = /dev/nst1a	Second SCSI tape, mode 3, no rewind
		    ...

		"No rewind" refers to the omission of the default
		automatic rewind on device close.  The MTREW or MTOFFL
		ioctl()'s can be used to rewind the tape regardless of
		the device used to access it.

   9 block	Metadisk (RAID) devices
		  0 = /dev/md0		First metadisk group
		  1 = /dev/md1		Second metadisk group
		    ...

		The metadisk driver is used to span a
		filesystem across multiple physical disks.

  10 char	Non-serial mice, misc features
		  0 = /dev/logibm	Logitech bus mouse
		  1 = /dev/psaux	PS/2-style mouse port
		  2 = /dev/inportbm	Microsoft Inport bus mouse
		  3 = /dev/atibm	ATI XL bus mouse
		  4 = /dev/jbm		J-mouse
		  4 = /dev/amigamouse	Amiga mouse (68k/Amiga)
		  5 = /dev/atarimouse	Atari mouse
		  6 = /dev/sunmouse	Sun mouse
		  7 = /dev/amigamouse1	Second Amiga mouse
		  8 = /dev/smouse	Simple serial mouse driver
		  9 = /dev/pc110pad	IBM PC-110 digitizer pad
		 10 = /dev/adbmouse	Apple Desktop Bus mouse
		 11 = /dev/vrtpanel	Vr41xx embedded touch panel
		 13 = /dev/vpcmouse	Connectix Virtual PC Mouse
		 14 = /dev/touchscreen/ucb1x00  UCB 1x00 touchscreen
		 15 = /dev/touchscreen/mk712	MK712 touchscreen
		128 = /dev/beep		Fancy beep device
		129 =
		130 = /dev/watchdog	Watchdog timer port
		131 = /dev/temperature	Machine internal temperature
		132 = /dev/hwtrap	Hardware fault trap
		133 = /dev/exttrp	External device trap
		134 = /dev/apm_bios	Advanced Power Management BIOS
		135 = /dev/rtc		Real Time Clock
		137 = /dev/vhci		Bluetooth virtual HCI driver
		139 = /dev/openprom	SPARC OpenBoot PROM
		140 = /dev/relay8	Berkshire Products Octal relay card
		141 = /dev/relay16	Berkshire Products ISO-16 relay card
		142 =
		143 = /dev/pciconf	PCI configuration space
		144 = /dev/nvram	Non-volatile configuration RAM
		145 = /dev/hfmodem	Soundcard shortwave modem control
		146 = /dev/graphics	Linux/SGI graphics device
		147 = /dev/opengl	Linux/SGI OpenGL pipe
		148 = /dev/gfx		Linux/SGI graphics effects device
		149 = /dev/input/mouse	Linux/SGI Irix emulation mouse
		150 = /dev/input/keyboard Linux/SGI Irix emulation keyboard
		151 = /dev/led		Front panel LEDs
		152 = /dev/kpoll	Kernel Poll Driver
		153 = /dev/mergemem	Memory merge device
		154 = /dev/pmu		Macintosh PowerBook power manager
		155 =
		156 = /dev/lcd		Front panel LCD display
		157 = /dev/ac		Applicom Intl Profibus card
		158 = /dev/nwbutton	Netwinder external button
		159 = /dev/nwdebug	Netwinder debug interface
		160 = /dev/nwflash	Netwinder flash memory
		161 = /dev/userdma	User-space DMA access
		162 = /dev/smbus	System Management Bus
		163 = /dev/lik		Logitech Internet Keyboard
		164 = /dev/ipmo		Intel Intelligent Platform Management
		165 = /dev/vmmon	VMware virtual machine monitor
		166 = /dev/i2o/ctl	I2O configuration manager
		167 = /dev/specialix_sxctl Specialix serial control
		168 = /dev/tcldrv	Technology Concepts serial control
		169 = /dev/specialix_rioctl Specialix RIO serial control
		170 = /dev/thinkpad/thinkpad	IBM Thinkpad devices
		171 = /dev/srripc	QNX4 API IPC manager
		172 = /dev/usemaclone	Semaphore clone device
		173 = /dev/ipmikcs	Intelligent Platform Management
		174 = /dev/uctrl	SPARCbook 3 microcontroller
		175 = /dev/agpgart	AGP Graphics Address Remapping Table
		176 = /dev/gtrsc	Gorgy Timing radio clock
		177 = /dev/cbm		Serial CBM bus
		178 = /dev/jsflash	JavaStation OS flash SIMM
		179 = /dev/xsvc		High-speed shared-mem/semaphore service
		180 = /dev/vrbuttons	Vr41xx button input device
		181 = /dev/toshiba	Toshiba laptop SMM support
		182 = /dev/perfctr	Performance-monitoring counters
		183 = /dev/hwrng	Generic random number generator
		184 = /dev/cpu/microcode CPU microcode update interface
		186 = /dev/atomicps	Atomic snapshot of process state data
		187 = /dev/irnet	IrNET device
		188 = /dev/smbusbios	SMBus BIOS
		189 = /dev/ussp_ctl	User space serial port control
		190 = /dev/crash	Mission Critical Linux crash dump facility
		191 = /dev/pcl181	<information missing>
		192 = /dev/nas_xbus	NAS xbus LCD/buttons access
		193 = /dev/d7s		SPARC 7-segment display
		194 = /dev/zkshim	Zero-Knowledge network shim control
		195 = /dev/elographics/e2201	Elographics touchscreen E271-2201
		196 = /dev/vfio/vfio	VFIO userspace driver interface
		197 = /dev/pxa3xx-gcu	PXA3xx graphics controller unit driver
		198 = /dev/sexec	Signed executable interface
		199 = /dev/scanners/cuecat :CueCat barcode scanner
		200 = /dev/net/tun	TAP/TUN network device
		201 = /dev/button/gulpb	Transmeta GULP-B buttons
		202 = /dev/emd/ctl	Enhanced Metadisk RAID (EMD) control
		203 = /dev/cuse		Cuse (character device in user-space)
		204 = /dev/video/em8300		EM8300 DVD decoder control
		205 = /dev/video/em8300_mv	EM8300 DVD decoder video
		206 = /dev/video/em8300_ma	EM8300 DVD decoder audio
		207 = /dev/video/em8300_sp	EM8300 DVD decoder subpicture
		208 = /dev/compaq/cpqphpc	Compaq PCI Hot Plug Controller
		209 = /dev/compaq/cpqrid	Compaq Remote Insight Driver
		210 = /dev/impi/bt	IMPI coprocessor block transfer
		211 = /dev/impi/smic	IMPI coprocessor stream interface
		212 = /dev/watchdogs/0	First watchdog device
		213 = /dev/watchdogs/1	Second watchdog device
		214 = /dev/watchdogs/2	Third watchdog device
		215 = /dev/watchdogs/3	Fourth watchdog device
		216 = /dev/fujitsu/apanel	Fujitsu/Siemens application panel
		217 = /dev/ni/natmotn		National Instruments Motion
		218 = /dev/kchuid	Inter-process chuid control
		219 = /dev/modems/mwave	MWave modem firmware upload
		220 = /dev/mptctl	Message passing technology (MPT) control
		221 = /dev/mvista/hssdsi	Montavista PICMG hot swap system driver
		222 = /dev/mvista/hasi		Montavista PICMG high availability
		223 = /dev/input/uinput		User level driver support for input
		224 = /dev/tpm		TCPA TPM driver
		225 = /dev/pps		Pulse Per Second driver
		226 = /dev/systrace	Systrace device
		227 = /dev/mcelog	X86_64 Machine Check Exception driver
		228 = /dev/hpet		HPET driver
		229 = /dev/fuse		Fuse (virtual filesystem in user-space)
		230 = /dev/midishare	MidiShare driver
		231 = /dev/snapshot	System memory snapshot device
		232 = /dev/kvm		Kernel-based virtual machine (hardware virtualization extensions)
		233 = /dev/kmview	View-OS A process with a view
		234 = /dev/btrfs-control	Btrfs control device
		235 = /dev/autofs	Autofs control device
		236 = /dev/mapper/control	Device-Mapper control device
		237 = /dev/loop-control Loopback control device
		238 = /dev/vhost-net	Host kernel accelerator for virtio net
		239 = /dev/uhid		User-space I/O driver support for HID subsystem
		240 = /dev/userio	Serio driver testing device
		241 = /dev/vhost-vsock	Host kernel driver for virtio vsock
		242 = /dev/rfkill	Turning off radio transmissions (rfkill)

		243-254			Reserved for local use
		255			Reserved for MISC_DYNAMIC_MINOR

  11 char	Raw keyboard device	(Linux/SPARC only)
		  0 = /dev/kbd		Raw keyboard device

  11 char	Serial Mux device	(Linux/PA-RISC only)
		  0 = /dev/ttyB0	First mux port
		  1 = /dev/ttyB1	Second mux port
		    ...

  11 block	SCSI CD-ROM devices
		  0 = /dev/scd0		First SCSI CD-ROM
		  1 = /dev/scd1		Second SCSI CD-ROM
		    ...

		The prefix /dev/sr (instead of /dev/scd) has been deprecated.

  12 char	QIC-02 tape
		  2 = /dev/ntpqic11	QIC-11, no rewind-on-close
		  3 = /dev/tpqic11	QIC-11, rewind-on-close
		  4 = /dev/ntpqic24	QIC-24, no rewind-on-close
		  5 = /dev/tpqic24	QIC-24, rewind-on-close
		  6 = /dev/ntpqic120	QIC-120, no rewind-on-close
		  7 = /dev/tpqic120	QIC-120, rewind-on-close
		  8 = /dev/ntpqic150	QIC-150, no rewind-on-close
		  9 = /dev/tpqic150	QIC-150, rewind-on-close

		The device names specified are proposed -- if there
		are "standard" names for these devices, please let me know.

  12 block

  13 char	Input core
		  0 = /dev/input/js0	First joystick
		  1 = /dev/input/js1	Second joystick
		    ...
		 32 = /dev/input/mouse0	First mouse
		 33 = /dev/input/mouse1	Second mouse
		    ...
		 63 = /dev/input/mice	Unified mouse
		 64 = /dev/input/event0	First event queue
		 65 = /dev/input/event1	Second event queue
		    ...

		Each device type has 5 bits (32 minors).

  13 block	Previously used for the XT disk (/dev/xdN)
		Deleted in kernel v3.9.

  14 char	Open Sound System (OSS)
		  0 = /dev/mixer	Mixer control
		  1 = /dev/sequencer	Audio sequencer
		  2 = /dev/midi00	First MIDI port
		  3 = /dev/dsp		Digital audio
		  4 = /dev/audio	Sun-compatible digital audio
		  6 =
		  7 = /dev/audioctl	SPARC audio control device
		  8 = /dev/sequencer2	Sequencer -- alternate device
		 16 = /dev/mixer1	Second soundcard mixer control
		 17 = /dev/patmgr0	Sequencer patch manager
		 18 = /dev/midi01	Second MIDI port
		 19 = /dev/dsp1		Second soundcard digital audio
		 20 = /dev/audio1	Second soundcard Sun digital audio
		 33 = /dev/patmgr1	Sequencer patch manager
		 34 = /dev/midi02	Third MIDI port
		 50 = /dev/midi03	Fourth MIDI port

  14 block

  15 char	Joystick
		  0 = /dev/js0		First analog joystick
		  1 = /dev/js1		Second analog joystick
		    ...
		128 = /dev/djs0		First digital joystick
		129 = /dev/djs1		Second digital joystick
		    ...
  15 block	Sony CDU-31A/CDU-33A CD-ROM
		  0 = /dev/sonycd	Sony CDU-31a CD-ROM

  16 char	Non-SCSI scanners
		  0 = /dev/gs4500	Genius 4500 handheld scanner

  16 block	GoldStar CD-ROM
		  0 = /dev/gscd		GoldStar CD-ROM

  17 char	OBSOLETE (was Chase serial card)
		  0 = /dev/ttyH0	First Chase port
		  1 = /dev/ttyH1	Second Chase port
		    ...
  17 block	Optics Storage CD-ROM
		  0 = /dev/optcd	Optics Storage CD-ROM

  18 char	OBSOLETE (was Chase serial card - alternate devices)
		  0 = /dev/cuh0		Callout device for ttyH0
		  1 = /dev/cuh1		Callout device for ttyH1
		    ...
  18 block	Sanyo CD-ROM
		  0 = /dev/sjcd		Sanyo CD-ROM

  19 block	"Double" compressed disk
		  0 = /dev/double0	First compressed disk
		    ...
		  7 = /dev/double7	Eighth compressed disk
		128 = /dev/cdouble0	Mirror of first compressed disk
		    ...
		135 = /dev/cdouble7	Mirror of eighth compressed disk

		See the Double documentation for the meaning of the
		mirror devices.

  20 block	Hitachi CD-ROM (under development)
		  0 = /dev/hitcd	Hitachi CD-ROM

  21 char	Generic SCSI access
		  0 = /dev/sg0		First generic SCSI device
		  1 = /dev/sg1		Second generic SCSI device
		    ...

		Most distributions name these /dev/sga, /dev/sgb...;
		this sets an unnecessary limit of 26 SCSI devices in
		the system and is counter to standard Linux
		device-naming practice.

  21 block	Acorn MFM hard drive interface
		  0 = /dev/mfma		First MFM drive whole disk
		 64 = /dev/mfmb		Second MFM drive whole disk

		This device is used on the ARM-based Acorn RiscPC.
		Partitions are handled the same way as for IDE disks
		(see major number 3).

  22 char	Digiboard serial card
		  0 = /dev/ttyD0	First Digiboard port
		  1 = /dev/ttyD1	Second Digiboard port
		    ...
  22 block	Second IDE hard disk/CD-ROM interface
		  0 = /dev/hdc		Master: whole disk (or CD-ROM)
		 64 = /dev/hdd		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  23 char	Digiboard serial card - alternate devices
		  0 = /dev/cud0		Callout device for ttyD0
		  1 = /dev/cud1		Callout device for ttyD1
		      ...
  23 block	Mitsumi proprietary CD-ROM
		  0 = /dev/mcd		Mitsumi CD-ROM

  24 char	Stallion serial card
		  0 = /dev/ttyE0	Stallion port 0 card 0
		  1 = /dev/ttyE1	Stallion port 1 card 0
		    ...
		 64 = /dev/ttyE64	Stallion port 0 card 1
		 65 = /dev/ttyE65	Stallion port 1 card 1
		      ...
		128 = /dev/ttyE128	Stallion port 0 card 2
		129 = /dev/ttyE129	Stallion port 1 card 2
		    ...
		192 = /dev/ttyE192	Stallion port 0 card 3
		193 = /dev/ttyE193	Stallion port 1 card 3
		    ...
  24 block	Sony CDU-535 CD-ROM
		  0 = /dev/cdu535	Sony CDU-535 CD-ROM

  25 char	Stallion serial card - alternate devices
		  0 = /dev/cue0		Callout device for ttyE0
		  1 = /dev/cue1		Callout device for ttyE1
		    ...
		 64 = /dev/cue64	Callout device for ttyE64
		 65 = /dev/cue65	Callout device for ttyE65
		    ...
		128 = /dev/cue128	Callout device for ttyE128
		129 = /dev/cue129	Callout device for ttyE129
		    ...
		192 = /dev/cue192	Callout device for ttyE192
		193 = /dev/cue193	Callout device for ttyE193
		      ...
  25 block	First Matsushita (Panasonic/SoundBlaster) CD-ROM
		  0 = /dev/sbpcd0	Panasonic CD-ROM controller 0 unit 0
		  1 = /dev/sbpcd1	Panasonic CD-ROM controller 0 unit 1
		  2 = /dev/sbpcd2	Panasonic CD-ROM controller 0 unit 2
		  3 = /dev/sbpcd3	Panasonic CD-ROM controller 0 unit 3

  26 char

  26 block	Second Matsushita (Panasonic/SoundBlaster) CD-ROM
		  0 = /dev/sbpcd4	Panasonic CD-ROM controller 1 unit 0
		  1 = /dev/sbpcd5	Panasonic CD-ROM controller 1 unit 1
		  2 = /dev/sbpcd6	Panasonic CD-ROM controller 1 unit 2
		  3 = /dev/sbpcd7	Panasonic CD-ROM controller 1 unit 3

  27 char	QIC-117 tape
		  0 = /dev/qft0		Unit 0, rewind-on-close
		  1 = /dev/qft1		Unit 1, rewind-on-close
		  2 = /dev/qft2		Unit 2, rewind-on-close
		  3 = /dev/qft3		Unit 3, rewind-on-close
		  4 = /dev/nqft0	Unit 0, no rewind-on-close
		  5 = /dev/nqft1	Unit 1, no rewind-on-close
		  6 = /dev/nqft2	Unit 2, no rewind-on-close
		  7 = /dev/nqft3	Unit 3, no rewind-on-close
		 16 = /dev/zqft0	Unit 0, rewind-on-close, compression
		 17 = /dev/zqft1	Unit 1, rewind-on-close, compression
		 18 = /dev/zqft2	Unit 2, rewind-on-close, compression
		 19 = /dev/zqft3	Unit 3, rewind-on-close, compression
		 20 = /dev/nzqft0	Unit 0, no rewind-on-close, compression
		 21 = /dev/nzqft1	Unit 1, no rewind-on-close, compression
		 22 = /dev/nzqft2	Unit 2, no rewind-on-close, compression
		 23 = /dev/nzqft3	Unit 3, no rewind-on-close, compression
		 32 = /dev/rawqft0	Unit 0, rewind-on-close, no file marks
		 33 = /dev/rawqft1	Unit 1, rewind-on-close, no file marks
		 34 = /dev/rawqft2	Unit 2, rewind-on-close, no file marks
		 35 = /dev/rawqft3	Unit 3, rewind-on-close, no file marks
		 36 = /dev/nrawqft0	Unit 0, no rewind-on-close, no file marks
		 37 = /dev/nrawqft1	Unit 1, no rewind-on-close, no file marks
		 38 = /dev/nrawqft2	Unit 2, no rewind-on-close, no file marks
		 39 = /dev/nrawqft3	Unit 3, no rewind-on-close, no file marks

  27 block	Third Matsushita (Panasonic/SoundBlaster) CD-ROM
		  0 = /dev/sbpcd8	Panasonic CD-ROM controller 2 unit 0
		  1 = /dev/sbpcd9	Panasonic CD-ROM controller 2 unit 1
		  2 = /dev/sbpcd10	Panasonic CD-ROM controller 2 unit 2
		  3 = /dev/sbpcd11	Panasonic CD-ROM controller 2 unit 3

  28 char	Stallion serial card - card programming
		  0 = /dev/staliomem0	First Stallion card I/O memory
		  1 = /dev/staliomem1	Second Stallion card I/O memory
		  2 = /dev/staliomem2	Third Stallion card I/O memory
		  3 = /dev/staliomem3	Fourth Stallion card I/O memory

  28 char	Atari SLM ACSI laser printer (68k/Atari)
		  0 = /dev/slm0		First SLM laser printer
		  1 = /dev/slm1		Second SLM laser printer
		    ...
  28 block	Fourth Matsushita (Panasonic/SoundBlaster) CD-ROM
		  0 = /dev/sbpcd12	Panasonic CD-ROM controller 3 unit 0
		  1 = /dev/sbpcd13	Panasonic CD-ROM controller 3 unit 1
		  2 = /dev/sbpcd14	Panasonic CD-ROM controller 3 unit 2
		  3 = /dev/sbpcd15	Panasonic CD-ROM controller 3 unit 3

  28 block	ACSI disk (68k/Atari)
		  0 = /dev/ada		First ACSI disk whole disk
		 16 = /dev/adb		Second ACSI disk whole disk
		 32 = /dev/adc		Third ACSI disk whole disk
		    ...
		240 = /dev/adp		16th ACSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15, like SCSI.

  29 char	Universal frame buffer
		  0 = /dev/fb0		First frame buffer
		  1 = /dev/fb1		Second frame buffer
		    ...
		 31 = /dev/fb31		32nd frame buffer

  29 block	Aztech/Orchid/Okano/Wearnes CD-ROM
		  0 = /dev/aztcd	Aztech CD-ROM

  30 char	iBCS-2 compatibility devices
		  0 = /dev/socksys	Socket access
		  1 = /dev/spx		SVR3 local X interface
		 32 = /dev/inet/ip	Network access
		 33 = /dev/inet/icmp
		 34 = /dev/inet/ggp
		 35 = /dev/inet/ipip
		 36 = /dev/inet/tcp
		 37 = /dev/inet/egp
		 38 = /dev/inet/pup
		 39 = /dev/inet/udp
		 40 = /dev/inet/idp
		 41 = /dev/inet/rawip

		Additionally, iBCS-2 requires the following links:

		/dev/ip -> /dev/inet/ip
		/dev/icmp -> /dev/inet/icmp
		/dev/ggp -> /dev/inet/ggp
		/dev/ipip -> /dev/inet/ipip
		/dev/tcp -> /dev/inet/tcp
		/dev/egp -> /dev/inet/egp
		/dev/pup -> /dev/inet/pup
		/dev/udp -> /dev/inet/udp
		/dev/idp -> /dev/inet/idp
		/dev/rawip -> /dev/inet/rawip
		/dev/inet/arp -> /dev/inet/udp
		/dev/inet/rip -> /dev/inet/udp
		/dev/nfsd -> /dev/socksys
		/dev/X0R -> /dev/null (? apparently not required ?)

  30 block	Philips LMS CM-205 CD-ROM
		  0 = /dev/cm205cd	Philips LMS CM-205 CD-ROM

		/dev/lmscd is an older name for this device.  This
		driver does not work with the CM-205MS CD-ROM.

  31 char	MPU-401 MIDI
		  0 = /dev/mpu401data	MPU-401 data port
		  1 = /dev/mpu401stat	MPU-401 status port

  31 block	ROM/flash memory card
		  0 = /dev/rom0		First ROM card (rw)
		      ...
		  7 = /dev/rom7		Eighth ROM card (rw)
		  8 = /dev/rrom0	First ROM card (ro)
		    ...
		 15 = /dev/rrom7	Eighth ROM card (ro)
		 16 = /dev/flash0	First flash memory card (rw)
		    ...
		 23 = /dev/flash7	Eighth flash memory card (rw)
		 24 = /dev/rflash0	First flash memory card (ro)
		    ...
		 31 = /dev/rflash7	Eighth flash memory card (ro)

		The read-write (rw) devices support back-caching
		written data in RAM, as well as writing to flash RAM
		devices.  The read-only devices (ro) support reading
		only.

  32 char	Specialix serial card
		  0 = /dev/ttyX0	First Specialix port
		  1 = /dev/ttyX1	Second Specialix port
		    ...
  32 block	Philips LMS CM-206 CD-ROM
		  0 = /dev/cm206cd	Philips LMS CM-206 CD-ROM

  33 char	Specialix serial card - alternate devices
		  0 = /dev/cux0		Callout device for ttyX0
		  1 = /dev/cux1		Callout device for ttyX1
		    ...
  33 block	Third IDE hard disk/CD-ROM interface
		  0 = /dev/hde		Master: whole disk (or CD-ROM)
		 64 = /dev/hdf		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  34 char	Z8530 HDLC driver
		  0 = /dev/scc0		First Z8530, first port
		  1 = /dev/scc1		First Z8530, second port
		  2 = /dev/scc2		Second Z8530, first port
		  3 = /dev/scc3		Second Z8530, second port
		    ...

		In a previous version these devices were named
		/dev/sc1 for /dev/scc0, /dev/sc2 for /dev/scc1, and so
		on.

  34 block	Fourth IDE hard disk/CD-ROM interface
		  0 = /dev/hdg		Master: whole disk (or CD-ROM)
		 64 = /dev/hdh		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  35 char	tclmidi MIDI driver
		  0 = /dev/midi0	First MIDI port, kernel timed
		  1 = /dev/midi1	Second MIDI port, kernel timed
		  2 = /dev/midi2	Third MIDI port, kernel timed
		  3 = /dev/midi3	Fourth MIDI port, kernel timed
		 64 = /dev/rmidi0	First MIDI port, untimed
		 65 = /dev/rmidi1	Second MIDI port, untimed
		 66 = /dev/rmidi2	Third MIDI port, untimed
		 67 = /dev/rmidi3	Fourth MIDI port, untimed
		128 = /dev/smpte0	First MIDI port, SMPTE timed
		129 = /dev/smpte1	Second MIDI port, SMPTE timed
		130 = /dev/smpte2	Third MIDI port, SMPTE timed
		131 = /dev/smpte3	Fourth MIDI port, SMPTE timed

  35 block	Slow memory ramdisk
		  0 = /dev/slram	Slow memory ramdisk

  36 char	Netlink support
		  0 = /dev/route	Routing, device updates, kernel to user
		  1 = /dev/skip		enSKIP security cache control
		  3 = /dev/fwmonitor	Firewall packet copies
		 16 = /dev/tap0		First Ethertap device
		    ...
		 31 = /dev/tap15	16th Ethertap device

  36 block	OBSOLETE (was MCA ESDI hard disk)

  37 char	IDE tape
		  0 = /dev/ht0		First IDE tape
		  1 = /dev/ht1		Second IDE tape
		    ...
		128 = /dev/nht0		First IDE tape, no rewind-on-close
		129 = /dev/nht1		Second IDE tape, no rewind-on-close
		    ...

		Currently, only one IDE tape drive is supported.

  37 block	Zorro II ramdisk
		  0 = /dev/z2ram	Zorro II ramdisk

  38 char	Myricom PCI Myrinet board
		  0 = /dev/mlanai0	First Myrinet board
		  1 = /dev/mlanai1	Second Myrinet board
		    ...

		This device is used for status query, board control
		and "user level packet I/O."  This board is also
		accessible as a standard networking "eth" device.

  38 block	OBSOLETE (was Linux/AP+)

  39 char	ML-16P experimental I/O board
		  0 = /dev/ml16pa-a0	First card, first analog channel
		  1 = /dev/ml16pa-a1	First card, second analog channel
		    ...
		 15 = /dev/ml16pa-a15	First card, 16th analog channel
		 16 = /dev/ml16pa-d	First card, digital lines
		 17 = /dev/ml16pa-c0	First card, first counter/timer
		 18 = /dev/ml16pa-c1	First card, second counter/timer
		 19 = /dev/ml16pa-c2	First card, third counter/timer
		 32 = /dev/ml16pb-a0	Second card, first analog channel
		 33 = /dev/ml16pb-a1	Second card, second analog channel
		    ...
		 47 = /dev/ml16pb-a15	Second card, 16th analog channel
		 48 = /dev/ml16pb-d	Second card, digital lines
		 49 = /dev/ml16pb-c0	Second card, first counter/timer
		 50 = /dev/ml16pb-c1	Second card, second counter/timer
		 51 = /dev/ml16pb-c2	Second card, third counter/timer
		      ...
  39 block

  40 char

  40 block

  41 char	Yet Another Micro Monitor
		  0 = /dev/yamm		Yet Another Micro Monitor

  41 block

  42 char	Demo/sample use

  42 block	Demo/sample use

		This number is intended for use in sample code, as
		well as a general "example" device number.  It
		should never be used for a device driver that is being
		distributed; either obtain an official number or use
		the local/experimental range.  The sudden addition or
		removal of a driver with this number should not cause
		ill effects to the system (bugs excepted.)

		IN PARTICULAR, ANY DISTRIBUTION WHICH CONTAINS A
		DEVICE DRIVER USING MAJOR NUMBER 42 IS NONCOMPLIANT.

  43 char	isdn4linux virtual modem
		  0 = /dev/ttyI0	First virtual modem
		    ...
		 63 = /dev/ttyI63	64th virtual modem

  43 block	Network block devices
		  0 = /dev/nb0		First network block device
		  1 = /dev/nb1		Second network block device
		    ...

		Network Block Device is somehow similar to loopback
		devices: If you read from it, it sends packet across
		network asking server for data. If you write to it, it
		sends packet telling server to write. It could be used
		to mounting filesystems over the net, swapping over
		the net, implementing block device in userland etc.

  44 char	isdn4linux virtual modem - alternate devices
		  0 = /dev/cui0		Callout device for ttyI0
		    ...
		 63 = /dev/cui63	Callout device for ttyI63

  44 block	Flash Translation Layer (FTL) filesystems
		  0 = /dev/ftla		FTL on first Memory Technology Device
		 16 = /dev/ftlb		FTL on second Memory Technology Device
		 32 = /dev/ftlc		FTL on third Memory Technology Device
		    ...
		240 = /dev/ftlp		FTL on 16th Memory Technology Device

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the partition
		limit is 15 rather than 63 per disk (same as SCSI.)

  45 char	isdn4linux ISDN BRI driver
		  0 = /dev/isdn0	First virtual B channel raw data
		    ...
		 63 = /dev/isdn63	64th virtual B channel raw data
		 64 = /dev/isdnctrl0	First channel control/debug
		    ...
		127 = /dev/isdnctrl63	64th channel control/debug

		128 = /dev/ippp0	First SyncPPP device
		    ...
		191 = /dev/ippp63	64th SyncPPP device

		255 = /dev/isdninfo	ISDN monitor interface

  45 block	Parallel port IDE disk devices
		  0 = /dev/pda		First parallel port IDE disk
		 16 = /dev/pdb		Second parallel port IDE disk
		 32 = /dev/pdc		Third parallel port IDE disk
		 48 = /dev/pdd		Fourth parallel port IDE disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the partition
		limit is 15 rather than 63 per disk.

  46 char	Comtrol Rocketport serial card
		  0 = /dev/ttyR0	First Rocketport port
		  1 = /dev/ttyR1	Second Rocketport port
		    ...
  46 block	Parallel port ATAPI CD-ROM devices
		  0 = /dev/pcd0		First parallel port ATAPI CD-ROM
		  1 = /dev/pcd1		Second parallel port ATAPI CD-ROM
		  2 = /dev/pcd2		Third parallel port ATAPI CD-ROM
		  3 = /dev/pcd3		Fourth parallel port ATAPI CD-ROM

  47 char	Comtrol Rocketport serial card - alternate devices
		  0 = /dev/cur0		Callout device for ttyR0
		  1 = /dev/cur1		Callout device for ttyR1
		    ...
  47 block	Parallel port ATAPI disk devices
		  0 = /dev/pf0		First parallel port ATAPI disk
		  1 = /dev/pf1		Second parallel port ATAPI disk
		  2 = /dev/pf2		Third parallel port ATAPI disk
		  3 = /dev/pf3		Fourth parallel port ATAPI disk

		This driver is intended for floppy disks and similar
		devices and hence does not support partitioning.

  48 char	SDL RISCom serial card
		  0 = /dev/ttyL0	First RISCom port
		  1 = /dev/ttyL1	Second RISCom port
		    ...
  48 block	Mylex DAC960 PCI RAID controller; first controller
		  0 = /dev/rd/c0d0	First disk, whole disk
		  8 = /dev/rd/c0d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c0d31	32nd disk, whole disk

		For partitions add:
		  0 = /dev/rd/c?d?	Whole disk
		  1 = /dev/rd/c?d?p1	First partition
		    ...
		  7 = /dev/rd/c?d?p7	Seventh partition

  49 char	SDL RISCom serial card - alternate devices
		  0 = /dev/cul0		Callout device for ttyL0
		  1 = /dev/cul1		Callout device for ttyL1
		    ...
  49 block	Mylex DAC960 PCI RAID controller; second controller
		  0 = /dev/rd/c1d0	First disk, whole disk
		  8 = /dev/rd/c1d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c1d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  50 char	Reserved for GLINT

  50 block	Mylex DAC960 PCI RAID controller; third controller
		  0 = /dev/rd/c2d0	First disk, whole disk
		  8 = /dev/rd/c2d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c2d31	32nd disk, whole disk

  51 char	Baycom radio modem OR Radio Tech BIM-XXX-RS232 radio modem
		  0 = /dev/bc0		First Baycom radio modem
		  1 = /dev/bc1		Second Baycom radio modem
		    ...
  51 block	Mylex DAC960 PCI RAID controller; fourth controller
		  0 = /dev/rd/c3d0	First disk, whole disk
		  8 = /dev/rd/c3d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c3d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  52 char	Spellcaster DataComm/BRI ISDN card
		  0 = /dev/dcbri0	First DataComm card
		  1 = /dev/dcbri1	Second DataComm card
		  2 = /dev/dcbri2	Third DataComm card
		  3 = /dev/dcbri3	Fourth DataComm card

  52 block	Mylex DAC960 PCI RAID controller; fifth controller
		  0 = /dev/rd/c4d0	First disk, whole disk
		  8 = /dev/rd/c4d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c4d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  53 char	BDM interface for remote debugging MC683xx microcontrollers
		  0 = /dev/pd_bdm0	PD BDM interface on lp0
		  1 = /dev/pd_bdm1	PD BDM interface on lp1
		  2 = /dev/pd_bdm2	PD BDM interface on lp2
		  4 = /dev/icd_bdm0	ICD BDM interface on lp0
		  5 = /dev/icd_bdm1	ICD BDM interface on lp1
		  6 = /dev/icd_bdm2	ICD BDM interface on lp2

		This device is used for the interfacing to the MC683xx
		microcontrollers via Background Debug Mode by use of a
		Parallel Port interface. PD is the Motorola Public
		Domain Interface and ICD is the commercial interface
		by P&E.

  53 block	Mylex DAC960 PCI RAID controller; sixth controller
		  0 = /dev/rd/c5d0	First disk, whole disk
		  8 = /dev/rd/c5d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c5d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  54 char	Electrocardiognosis Holter serial card
		  0 = /dev/holter0	First Holter port
		  1 = /dev/holter1	Second Holter port
		  2 = /dev/holter2	Third Holter port

		A custom serial card used by Electrocardiognosis SRL
		<mseritan@ottonel.pub.ro> to transfer data from Holter
		24-hour heart monitoring equipment.

  54 block	Mylex DAC960 PCI RAID controller; seventh controller
		  0 = /dev/rd/c6d0	First disk, whole disk
		  8 = /dev/rd/c6d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c6d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  55 char	DSP56001 digital signal processor
		  0 = /dev/dsp56k	First DSP56001

  55 block	Mylex DAC960 PCI RAID controller; eighth controller
		  0 = /dev/rd/c7d0	First disk, whole disk
		  8 = /dev/rd/c7d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c7d31	32nd disk, whole disk

		Partitions are handled as for major 48.

  56 char	Apple Desktop Bus
		  0 = /dev/adb		ADB bus control

		Additional devices will be added to this number, all
		starting with /dev/adb.

  56 block	Fifth IDE hard disk/CD-ROM interface
		  0 = /dev/hdi		Master: whole disk (or CD-ROM)
		 64 = /dev/hdj		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  57 char	Hayes ESP serial card
		  0 = /dev/ttyP0	First ESP port
		  1 = /dev/ttyP1	Second ESP port
		    ...

  57 block	Sixth IDE hard disk/CD-ROM interface
		  0 = /dev/hdk		Master: whole disk (or CD-ROM)
		 64 = /dev/hdl		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  58 char	Hayes ESP serial card - alternate devices
		  0 = /dev/cup0		Callout device for ttyP0
		  1 = /dev/cup1		Callout device for ttyP1
		    ...

  58 block	Reserved for logical volume manager

  59 char	sf firewall package
		  0 = /dev/firewall	Communication with sf kernel module

  59 block	Generic PDA filesystem device
		  0 = /dev/pda0		First PDA device
		  1 = /dev/pda1		Second PDA device
		    ...

		The pda devices are used to mount filesystems on
		remote pda's (basically slow handheld machines with
		proprietary OS's and limited memory and storage
		running small fs translation drivers) through serial /
		IRDA / parallel links.

		NAMING CONFLICT -- PROPOSED REVISED NAME /dev/rpda0 etc

  60-63 char	LOCAL/EXPERIMENTAL USE

  60-63 block	LOCAL/EXPERIMENTAL USE
		Allocated for local/experimental use.  For devices not
		assigned official numbers, these ranges should be
		used in order to avoid conflicting with future assignments.

  64 char	ENskip kernel encryption package
		  0 = /dev/enskip	Communication with ENskip kernel module

  64 block	Scramdisk/DriveCrypt encrypted devices
		  0 = /dev/scramdisk/master    Master node for ioctls
		  1 = /dev/scramdisk/1         First encrypted device
		  2 = /dev/scramdisk/2         Second encrypted device
		  ...
		255 = /dev/scramdisk/255       255th encrypted device

		The filename of the encrypted container and the passwords
		are sent via ioctls (using the sdmount tool) to the master
		node which then activates them via one of the
		/dev/scramdisk/x nodes for loop mounting (all handled
		through the sdmount tool).

		Requested by: andy@scramdisklinux.org

  65 char	Sundance "plink" Transputer boards (obsolete, unused)
		  0 = /dev/plink0	First plink device
		  1 = /dev/plink1	Second plink device
		  2 = /dev/plink2	Third plink device
		  3 = /dev/plink3	Fourth plink device
		 64 = /dev/rplink0	First plink device, raw
		 65 = /dev/rplink1	Second plink device, raw
		 66 = /dev/rplink2	Third plink device, raw
		 67 = /dev/rplink3	Fourth plink device, raw
		128 = /dev/plink0d	First plink device, debug
		129 = /dev/plink1d	Second plink device, debug
		130 = /dev/plink2d	Third plink device, debug
		131 = /dev/plink3d	Fourth plink device, debug
		192 = /dev/rplink0d	First plink device, raw, debug
		193 = /dev/rplink1d	Second plink device, raw, debug
		194 = /dev/rplink2d	Third plink device, raw, debug
		195 = /dev/rplink3d	Fourth plink device, raw, debug

		This is a commercial driver; contact James Howes
		<jth@prosig.demon.co.uk> for information.

  65 block	SCSI disk devices (16-31)
		  0 = /dev/sdq		17th SCSI disk whole disk
		 16 = /dev/sdr		18th SCSI disk whole disk
		 32 = /dev/sds		19th SCSI disk whole disk
		    ...
		240 = /dev/sdaf		32nd SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  66 char	YARC PowerPC PCI coprocessor card
		  0 = /dev/yppcpci0	First YARC card
		  1 = /dev/yppcpci1	Second YARC card
		    ...

  66 block	SCSI disk devices (32-47)
		  0 = /dev/sdag		33th SCSI disk whole disk
		 16 = /dev/sdah		34th SCSI disk whole disk
		 32 = /dev/sdai		35th SCSI disk whole disk
		    ...
		240 = /dev/sdav		48nd SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  67 char	Coda network file system
		  0 = /dev/cfs0		Coda cache manager

		See http://www.coda.cs.cmu.edu for information about Coda.

  67 block	SCSI disk devices (48-63)
		  0 = /dev/sdaw		49th SCSI disk whole disk
		 16 = /dev/sdax		50th SCSI disk whole disk
		 32 = /dev/sday		51st SCSI disk whole disk
		    ...
		240 = /dev/sdbl		64th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  68 char	CAPI 2.0 interface
		  0 = /dev/capi20	Control device
		  1 = /dev/capi20.00	First CAPI 2.0 application
		  2 = /dev/capi20.01	Second CAPI 2.0 application
		    ...
		 20 = /dev/capi20.19	19th CAPI 2.0 application

		ISDN CAPI 2.0 driver for use with CAPI 2.0
		applications; currently supports the AVM B1 card.

  68 block	SCSI disk devices (64-79)
		  0 = /dev/sdbm		65th SCSI disk whole disk
		 16 = /dev/sdbn		66th SCSI disk whole disk
		 32 = /dev/sdbo		67th SCSI disk whole disk
		    ...
		240 = /dev/sdcb		80th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  69 char	MA16 numeric accelerator card
		  0 = /dev/ma16		Board memory access

  69 block	SCSI disk devices (80-95)
		  0 = /dev/sdcc		81st SCSI disk whole disk
		 16 = /dev/sdcd		82nd SCSI disk whole disk
		 32 = /dev/sdce		83th SCSI disk whole disk
		    ...
		240 = /dev/sdcr		96th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  70 char	SpellCaster Protocol Services Interface
		  0 = /dev/apscfg	Configuration interface
		  1 = /dev/apsauth	Authentication interface
		  2 = /dev/apslog	Logging interface
		  3 = /dev/apsdbg	Debugging interface
		 64 = /dev/apsisdn	ISDN command interface
		 65 = /dev/apsasync	Async command interface
		128 = /dev/apsmon	Monitor interface

  70 block	SCSI disk devices (96-111)
		  0 = /dev/sdcs		97th SCSI disk whole disk
		 16 = /dev/sdct		98th SCSI disk whole disk
		 32 = /dev/sdcu		99th SCSI disk whole disk
		    ...
		240 = /dev/sddh		112nd SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  71 char	Computone IntelliPort II serial card
		  0 = /dev/ttyF0	IntelliPort II board 0, port 0
		  1 = /dev/ttyF1	IntelliPort II board 0, port 1
		    ...
		 63 = /dev/ttyF63	IntelliPort II board 0, port 63
		 64 = /dev/ttyF64	IntelliPort II board 1, port 0
		 65 = /dev/ttyF65	IntelliPort II board 1, port 1
		    ...
		127 = /dev/ttyF127	IntelliPort II board 1, port 63
		128 = /dev/ttyF128	IntelliPort II board 2, port 0
		129 = /dev/ttyF129	IntelliPort II board 2, port 1
		    ...
		191 = /dev/ttyF191	IntelliPort II board 2, port 63
		192 = /dev/ttyF192	IntelliPort II board 3, port 0
		193 = /dev/ttyF193	IntelliPort II board 3, port 1
		    ...
		255 = /dev/ttyF255	IntelliPort II board 3, port 63

  71 block	SCSI disk devices (112-127)
		  0 = /dev/sddi		113th SCSI disk whole disk
		 16 = /dev/sddj		114th SCSI disk whole disk
		 32 = /dev/sddk		115th SCSI disk whole disk
		    ...
		240 = /dev/sddx		128th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  72 char	Computone IntelliPort II serial card - alternate devices
		  0 = /dev/cuf0		Callout device for ttyF0
		  1 = /dev/cuf1		Callout device for ttyF1
		    ...
		 63 = /dev/cuf63	Callout device for ttyF63
		 64 = /dev/cuf64	Callout device for ttyF64
		 65 = /dev/cuf65	Callout device for ttyF65
		    ...
		127 = /dev/cuf127	Callout device for ttyF127
		128 = /dev/cuf128	Callout device for ttyF128
		129 = /dev/cuf129	Callout device for ttyF129
		    ...
		191 = /dev/cuf191	Callout device for ttyF191
		192 = /dev/cuf192	Callout device for ttyF192
		193 = /dev/cuf193	Callout device for ttyF193
		    ...
		255 = /dev/cuf255	Callout device for ttyF255

  72 block	Compaq Intelligent Drive Array, first controller
		  0 = /dev/ida/c0d0	First logical drive whole disk
		 16 = /dev/ida/c0d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c0d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  73 char	Computone IntelliPort II serial card - control devices
		  0 = /dev/ip2ipl0	Loadware device for board 0
		  1 = /dev/ip2stat0	Status device for board 0
		  4 = /dev/ip2ipl1	Loadware device for board 1
		  5 = /dev/ip2stat1	Status device for board 1
		  8 = /dev/ip2ipl2	Loadware device for board 2
		  9 = /dev/ip2stat2	Status device for board 2
		 12 = /dev/ip2ipl3	Loadware device for board 3
		 13 = /dev/ip2stat3	Status device for board 3

  73 block	Compaq Intelligent Drive Array, second controller
		  0 = /dev/ida/c1d0	First logical drive whole disk
		 16 = /dev/ida/c1d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c1d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  74 char	SCI bridge
		  0 = /dev/SCI/0	SCI device 0
		  1 = /dev/SCI/1	SCI device 1
		    ...

		Currently for Dolphin Interconnect Solutions' PCI-SCI
		bridge.

  74 block	Compaq Intelligent Drive Array, third controller
		  0 = /dev/ida/c2d0	First logical drive whole disk
		 16 = /dev/ida/c2d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c2d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  75 char	Specialix IO8+ serial card
		  0 = /dev/ttyW0	First IO8+ port, first card
		  1 = /dev/ttyW1	Second IO8+ port, first card
		    ...
		  8 = /dev/ttyW8	First IO8+ port, second card
		    ...

  75 block	Compaq Intelligent Drive Array, fourth controller
		  0 = /dev/ida/c3d0	First logical drive whole disk
		 16 = /dev/ida/c3d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c3d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  76 char	Specialix IO8+ serial card - alternate devices
		  0 = /dev/cuw0		Callout device for ttyW0
		  1 = /dev/cuw1		Callout device for ttyW1
		    ...
		  8 = /dev/cuw8		Callout device for ttyW8
		    ...

  76 block	Compaq Intelligent Drive Array, fifth controller
		  0 = /dev/ida/c4d0	First logical drive whole disk
		 16 = /dev/ida/c4d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c4d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.


  77 char	ComScire Quantum Noise Generator
		  0 = /dev/qng		ComScire Quantum Noise Generator

  77 block	Compaq Intelligent Drive Array, sixth controller
		  0 = /dev/ida/c5d0	First logical drive whole disk
		 16 = /dev/ida/c5d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c5d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  78 char	PAM Software's multimodem boards
		  0 = /dev/ttyM0	First PAM modem
		  1 = /dev/ttyM1	Second PAM modem
		    ...

  78 block	Compaq Intelligent Drive Array, seventh controller
		  0 = /dev/ida/c6d0	First logical drive whole disk
		 16 = /dev/ida/c6d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c6d15	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  79 char	PAM Software's multimodem boards - alternate devices
		  0 = /dev/cum0		Callout device for ttyM0
		  1 = /dev/cum1		Callout device for ttyM1
		    ...

  79 block	Compaq Intelligent Drive Array, eighth controller
		  0 = /dev/ida/c7d0	First logical drive whole disk
		 16 = /dev/ida/c7d1	Second logical drive whole disk
		    ...
		240 = /dev/ida/c715	16th logical drive whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

  80 char	Photometrics AT200 CCD camera
		  0 = /dev/at200	Photometrics AT200 CCD camera

  80 block	I2O hard disk
		  0 = /dev/i2o/hda	First I2O hard disk, whole disk
		 16 = /dev/i2o/hdb	Second I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdp	16th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  81 char	video4linux
		  0 = /dev/video0	Video capture/overlay device
		    ...
		 63 = /dev/video63	Video capture/overlay device
		 64 = /dev/radio0	Radio device
		    ...
		127 = /dev/radio63	Radio device
		128 = /dev/swradio0	Software Defined Radio device
		    ...
		191 = /dev/swradio63	Software Defined Radio device
		224 = /dev/vbi0		Vertical blank interrupt
		    ...
		255 = /dev/vbi31	Vertical blank interrupt

		Minor numbers are allocated dynamically unless
		CONFIG_VIDEO_FIXED_MINOR_RANGES (default n)
		configuration option is set.

  81 block	I2O hard disk
		  0 = /dev/i2o/hdq	17th I2O hard disk, whole disk
		 16 = /dev/i2o/hdr	18th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdaf	32nd I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  82 char	WiNRADiO communications receiver card
		  0 = /dev/winradio0	First WiNRADiO card
		  1 = /dev/winradio1	Second WiNRADiO card
		    ...

		The driver and documentation may be obtained from
		https://www.winradio.com/

  82 block	I2O hard disk
		  0 = /dev/i2o/hdag	33rd I2O hard disk, whole disk
		 16 = /dev/i2o/hdah	34th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdav	48th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  83 char	Matrox mga_vid video driver
		 0 = /dev/mga_vid0	1st video card
		 1 = /dev/mga_vid1	2nd video card
		 2 = /dev/mga_vid2	3rd video card
		  ...
		15 = /dev/mga_vid15	16th video card

  83 block	I2O hard disk
		  0 = /dev/i2o/hdaw	49th I2O hard disk, whole disk
		 16 = /dev/i2o/hdax	50th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdbl	64th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  84 char	Ikon 1011[57] Versatec Greensheet Interface
		  0 = /dev/ihcp0	First Greensheet port
		  1 = /dev/ihcp1	Second Greensheet port

  84 block	I2O hard disk
		  0 = /dev/i2o/hdbm	65th I2O hard disk, whole disk
		 16 = /dev/i2o/hdbn	66th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdcb	80th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  85 char	Linux/SGI shared memory input queue
		  0 = /dev/shmiq	Master shared input queue
		  1 = /dev/qcntl0	First device pushed
		  2 = /dev/qcntl1	Second device pushed
		    ...

  85 block	I2O hard disk
		  0 = /dev/i2o/hdcc	81st I2O hard disk, whole disk
		 16 = /dev/i2o/hdcd	82nd I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hdcr	96th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  86 char	SCSI media changer
		  0 = /dev/sch0		First SCSI media changer
		  1 = /dev/sch1		Second SCSI media changer
		    ...

  86 block	I2O hard disk
		  0 = /dev/i2o/hdcs	97th I2O hard disk, whole disk
		 16 = /dev/i2o/hdct	98th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hddh	112th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  87 char	Sony Control-A1 stereo control bus
		  0 = /dev/controla0	First device on chain
		  1 = /dev/controla1	Second device on chain
		    ...

  87 block	I2O hard disk
		  0 = /dev/i2o/hddi	113rd I2O hard disk, whole disk
		 16 = /dev/i2o/hddj	114th I2O hard disk, whole disk
		    ...
		240 = /dev/i2o/hddx	128th I2O hard disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  88 char	COMX synchronous serial card
		  0 = /dev/comx0	COMX channel 0
		  1 = /dev/comx1	COMX channel 1
		    ...

  88 block	Seventh IDE hard disk/CD-ROM interface
		  0 = /dev/hdm		Master: whole disk (or CD-ROM)
		 64 = /dev/hdn		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  89 char	I2C bus interface
		  0 = /dev/i2c-0	First I2C adapter
		  1 = /dev/i2c-1	Second I2C adapter
		    ...

  89 block	Eighth IDE hard disk/CD-ROM interface
		  0 = /dev/hdo		Master: whole disk (or CD-ROM)
		 64 = /dev/hdp		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  90 char	Memory Technology Device (RAM, ROM, Flash)
		  0 = /dev/mtd0		First MTD (rw)
		  1 = /dev/mtdr0	First MTD (ro)
		    ...
		 30 = /dev/mtd15	16th MTD (rw)
		 31 = /dev/mtdr15	16th MTD (ro)

  90 block	Ninth IDE hard disk/CD-ROM interface
		  0 = /dev/hdq		Master: whole disk (or CD-ROM)
		 64 = /dev/hdr		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  91 char	CAN-Bus devices
		  0 = /dev/can0		First CAN-Bus controller
		  1 = /dev/can1		Second CAN-Bus controller
		    ...

  91 block	Tenth IDE hard disk/CD-ROM interface
		  0 = /dev/hds		Master: whole disk (or CD-ROM)
		 64 = /dev/hdt		Slave: whole disk (or CD-ROM)

		Partitions are handled the same way as for the first
		interface (see major number 3).

  92 char	Reserved for ith Kommunikationstechnik MIC ISDN card

  92 block	PPDD encrypted disk driver
		  0 = /dev/ppdd0	First encrypted disk
		  1 = /dev/ppdd1	Second encrypted disk
		    ...

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

  93 char

  93 block	NAND Flash Translation Layer filesystem
		  0 = /dev/nftla	First NFTL layer
		 16 = /dev/nftlb	Second NFTL layer
		    ...
		240 = /dev/nftlp	16th NTFL layer

  94 char

  94 block	IBM S/390 DASD block storage
		  0 = /dev/dasda First DASD device, major
		  1 = /dev/dasda1 First DASD device, block 1
		  2 = /dev/dasda2 First DASD device, block 2
		  3 = /dev/dasda3 First DASD device, block 3
		  4 = /dev/dasdb Second DASD device, major
		  5 = /dev/dasdb1 Second DASD device, block 1
		  6 = /dev/dasdb2 Second DASD device, block 2
		  7 = /dev/dasdb3 Second DASD device, block 3
		    ...

  95 char	IP filter
		  0 = /dev/ipl		Filter control device/log file
		  1 = /dev/ipnat	NAT control device/log file
		  2 = /dev/ipstate	State information log file
		  3 = /dev/ipauth	Authentication control device/log file
		    ...

  96 char	Parallel port ATAPI tape devices
		  0 = /dev/pt0		First parallel port ATAPI tape
		  1 = /dev/pt1		Second parallel port ATAPI tape
		    ...
		128 = /dev/npt0		First p.p. ATAPI tape, no rewind
		129 = /dev/npt1		Second p.p. ATAPI tape, no rewind
		    ...

  96 block	Inverse NAND Flash Translation Layer
		  0 = /dev/inftla First INFTL layer
		 16 = /dev/inftlb Second INFTL layer
		    ...
		240 = /dev/inftlp	16th INTFL layer

  97 char	Parallel port generic ATAPI interface
		  0 = /dev/pg0		First parallel port ATAPI device
		  1 = /dev/pg1		Second parallel port ATAPI device
		  2 = /dev/pg2		Third parallel port ATAPI device
		  3 = /dev/pg3		Fourth parallel port ATAPI device

		These devices support the same API as the generic SCSI
		devices.

  98 char	Control and Measurement Device (comedi)
		  0 = /dev/comedi0	First comedi device
		  1 = /dev/comedi1	Second comedi device
		    ...
		 47 = /dev/comedi47	48th comedi device

		Minors 48 to 255 are reserved for comedi subdevices with
		pathnames of the form "/dev/comediX_subdY", where "X" is the
		minor number of the associated comedi device and "Y" is the
		subdevice number.  These subdevice minors are assigned
		dynamically, so there is no fixed mapping from subdevice
		pathnames to minor numbers.

		See https://www.comedi.org/ for information about the Comedi
		project.

  98 block	User-mode virtual block device
		  0 = /dev/ubda		First user-mode block device
		 16 = /dev/ubdb		Second user-mode block device
		    ...

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

		This device is used by the user-mode virtual kernel port.

  99 char	Raw parallel ports
		  0 = /dev/parport0	First parallel port
		  1 = /dev/parport1	Second parallel port
		    ...

  99 block	JavaStation flash disk
		  0 = /dev/jsfd		JavaStation flash disk

 100 char	Telephony for Linux
		  0 = /dev/phone0	First telephony device
		  1 = /dev/phone1	Second telephony device
		    ...

 101 char	Motorola DSP 56xxx board
		  0 = /dev/mdspstat	Status information
		  1 = /dev/mdsp1	First DSP board I/O controls
		    ...
		 16 = /dev/mdsp16	16th DSP board I/O controls

 101 block	AMI HyperDisk RAID controller
		  0 = /dev/amiraid/ar0	First array whole disk
		 16 = /dev/amiraid/ar1	Second array whole disk
		    ...
		240 = /dev/amiraid/ar15	16th array whole disk

		For each device, partitions are added as:
		  0 = /dev/amiraid/ar?	  Whole disk
		  1 = /dev/amiraid/ar?p1  First partition
		  2 = /dev/amiraid/ar?p2  Second partition
		    ...
		 15 = /dev/amiraid/ar?p15 15th partition

 102 char

 102 block	Compressed block device
		  0 = /dev/cbd/a	First compressed block device, whole device
		 16 = /dev/cbd/b	Second compressed block device, whole device
		    ...
		240 = /dev/cbd/p	16th compressed block device, whole device

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 103 char	Arla network file system
		  0 = /dev/nnpfs0	First NNPFS device
		  1 = /dev/nnpfs1	Second NNPFS device

		Arla is a free clone of the Andrew File System, AFS.
		The NNPFS device gives user mode filesystem
		implementations a kernel presence for caching and easy
		mounting.  For more information about the project,
		write to <arla-drinkers@stacken.kth.se> or see
		https://www.stacken.kth.se/project/arla/

 103 block	Audit device
		  0 = /dev/audit	Audit device

 104 char	Flash BIOS support

 104 block	Compaq Next Generation Drive Array, first controller
		  0 = /dev/cciss/c0d0	First logical drive, whole disk
		 16 = /dev/cciss/c0d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c0d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 105 char	Comtrol VS-1000 serial controller
		  0 = /dev/ttyV0	First VS-1000 port
		  1 = /dev/ttyV1	Second VS-1000 port
		    ...

 105 block	Compaq Next Generation Drive Array, second controller
		  0 = /dev/cciss/c1d0	First logical drive, whole disk
		 16 = /dev/cciss/c1d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c1d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 106 char	Comtrol VS-1000 serial controller - alternate devices
		  0 = /dev/cuv0		First VS-1000 port
		  1 = /dev/cuv1		Second VS-1000 port
		    ...

 106 block	Compaq Next Generation Drive Array, third controller
		  0 = /dev/cciss/c2d0	First logical drive, whole disk
		 16 = /dev/cciss/c2d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c2d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 107 char	3Dfx Voodoo Graphics device
		  0 = /dev/3dfx		Primary 3Dfx graphics device

 107 block	Compaq Next Generation Drive Array, fourth controller
		  0 = /dev/cciss/c3d0	First logical drive, whole disk
		 16 = /dev/cciss/c3d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c3d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 108 char	Device independent PPP interface
		  0 = /dev/ppp		Device independent PPP interface

 108 block	Compaq Next Generation Drive Array, fifth controller
		  0 = /dev/cciss/c4d0	First logical drive, whole disk
		 16 = /dev/cciss/c4d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c4d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 109 char	Reserved for logical volume manager

 109 block	Compaq Next Generation Drive Array, sixth controller
		  0 = /dev/cciss/c5d0	First logical drive, whole disk
		 16 = /dev/cciss/c5d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c5d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 110 char	miroMEDIA Surround board
		  0 = /dev/srnd0	First miroMEDIA Surround board
		  1 = /dev/srnd1	Second miroMEDIA Surround board
		    ...

 110 block	Compaq Next Generation Drive Array, seventh controller
		  0 = /dev/cciss/c6d0	First logical drive, whole disk
		 16 = /dev/cciss/c6d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c6d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 111 char

 111 block	Compaq Next Generation Drive Array, eighth controller
		  0 = /dev/cciss/c7d0	First logical drive, whole disk
		 16 = /dev/cciss/c7d1	Second logical drive, whole disk
		    ...
		240 = /dev/cciss/c7d15	16th logical drive, whole disk

		Partitions are handled the same way as for Mylex
		DAC960 (see major number 48) except that the limit on
		partitions is 15.

 112 char	ISI serial card
		  0 = /dev/ttyM0	First ISI port
		  1 = /dev/ttyM1	Second ISI port
		    ...

		There is currently a device-naming conflict between
		these and PAM multimodems (major 78).

 112 block	IBM iSeries virtual disk
		  0 = /dev/iseries/vda	First virtual disk, whole disk
		  8 = /dev/iseries/vdb	Second virtual disk, whole disk
		    ...
		200 = /dev/iseries/vdz	26th virtual disk, whole disk
		208 = /dev/iseries/vdaa	27th virtual disk, whole disk
		    ...
		248 = /dev/iseries/vdaf	32nd virtual disk, whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 7.

 113 char	ISI serial card - alternate devices
		  0 = /dev/cum0		Callout device for ttyM0
		  1 = /dev/cum1		Callout device for ttyM1
		    ...

 113 block	IBM iSeries virtual CD-ROM
		  0 = /dev/iseries/vcda	First virtual CD-ROM
		  1 = /dev/iseries/vcdb	Second virtual CD-ROM
		    ...

 114 char	Picture Elements ISE board
		  0 = /dev/ise0		First ISE board
		  1 = /dev/ise1		Second ISE board
		    ...
		128 = /dev/isex0	Control node for first ISE board
		129 = /dev/isex1	Control node for second ISE board
		    ...

		The ISE board is an embedded computer, optimized for
		image processing. The /dev/iseN nodes are the general
		I/O access to the board, the /dev/isex0 nodes command
		nodes used to control the board.

 114 block       IDE BIOS powered software RAID interfaces such as the
		Promise Fastrak

		   0 = /dev/ataraid/d0
		   1 = /dev/ataraid/d0p1
		   2 = /dev/ataraid/d0p2
		  ...
		  16 = /dev/ataraid/d1
		  17 = /dev/ataraid/d1p1
		  18 = /dev/ataraid/d1p2
		  ...
		 255 = /dev/ataraid/d15p15

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 115 char	TI link cable devices (115 was formerly the console driver speaker)
		  0 = /dev/tipar0    Parallel cable on first parallel port
		  ...
		  7 = /dev/tipar7    Parallel cable on seventh parallel port

		  8 = /dev/tiser0    Serial cable on first serial port
		  ...
		 15 = /dev/tiser7    Serial cable on seventh serial port

		 16 = /dev/tiusb0    First USB cable
		  ...
		 47 = /dev/tiusb31   32nd USB cable

 115 block       NetWare (NWFS) Devices (0-255)

		The NWFS (NetWare) devices are used to present a
		collection of NetWare Mirror Groups or NetWare
		Partitions as a logical storage segment for
		use in mounting NetWare volumes.  A maximum of
		 256 NetWare volumes can be supported in a single
		machine.

		http://cgfa.telepac.pt/ftp2/kernel.org/linux/kernel/people/jmerkey/nwfs/

		 0 = /dev/nwfs/v0    First NetWare (NWFS) Logical Volume
		 1 = /dev/nwfs/v1    Second NetWare (NWFS) Logical Volume
		 2 = /dev/nwfs/v2    Third NetWare (NWFS) Logical Volume
		      ...
		 255 = /dev/nwfs/v255    Last NetWare (NWFS) Logical Volume

 116 char	Advanced Linux Sound Driver (ALSA)

 116 block       MicroMemory battery backed RAM adapter (NVRAM)
		Supports 16 boards, 15 partitions each.
		Requested by neilb at cse.unsw.edu.au.

		 0 = /dev/umem/d0      Whole of first board
		 1 = /dev/umem/d0p1    First partition of first board
		 2 = /dev/umem/d0p2    Second partition of first board
		15 = /dev/umem/d0p15   15th partition of first board

		16 = /dev/umem/d1      Whole of second board
		17 = /dev/umem/d1p1    First partition of second board
		    ...
		255= /dev/umem/d15p15  15th partition of 16th board.

 117 char	[REMOVED] COSA/SRP synchronous serial card
		  0 = /dev/cosa0c0	1st board, 1st channel
		  1 = /dev/cosa0c1	1st board, 2nd channel
		    ...
		 16 = /dev/cosa1c0	2nd board, 1st channel
		 17 = /dev/cosa1c1	2nd board, 2nd channel
		    ...

 117 block       Enterprise Volume Management System (EVMS)

		The EVMS driver uses a layered, plug-in model to provide
		unparalleled flexibility and extensibility in managing
		storage.  This allows for easy expansion or customization
		of various levels of volume management.  Requested by
		Mark Peloquin (peloquin at us.ibm.com).

		Note: EVMS populates and manages all the devnodes in
		/dev/evms.

		http://sf.net/projects/evms

		   0 = /dev/evms/block_device   EVMS block device
		   1 = /dev/evms/legacyname1    First EVMS legacy device
		   2 = /dev/evms/legacyname2    Second EVMS legacy device
		    ...
		    Both ranges can grow (down or up) until they meet.
		    ...
		 254 = /dev/evms/EVMSname2      Second EVMS native device
		 255 = /dev/evms/EVMSname1      First EVMS native device

		Note: legacyname(s) are derived from the normal legacy
		device names.  For example, /dev/hda5 would become
		/dev/evms/hda5.

 118 char	IBM Cryptographic Accelerator
		  0 = /dev/ica	Virtual interface to all IBM Crypto Accelerators
		  1 = /dev/ica0	IBMCA Device 0
		  2 = /dev/ica1	IBMCA Device 1
		    ...

 119 char	VMware virtual network control
		  0 = /dev/vnet0	1st virtual network
		  1 = /dev/vnet1	2nd virtual network
		    ...

 120-127 char	LOCAL/EXPERIMENTAL USE

 120-127 block	LOCAL/EXPERIMENTAL USE
		Allocated for local/experimental use.  For devices not
		assigned official numbers, these ranges should be
		used in order to avoid conflicting with future assignments.

 128-135 char	Unix98 PTY masters

		These devices should not have corresponding device
		nodes; instead they should be accessed through the
		/dev/ptmx cloning interface.

 128 block       SCSI disk devices (128-143)
		   0 = /dev/sddy         129th SCSI disk whole disk
		  16 = /dev/sddz         130th SCSI disk whole disk
		  32 = /dev/sdea         131th SCSI disk whole disk
		    ...
		 240 = /dev/sden         144th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 129 block       SCSI disk devices (144-159)
		   0 = /dev/sdeo         145th SCSI disk whole disk
		  16 = /dev/sdep         146th SCSI disk whole disk
		  32 = /dev/sdeq         147th SCSI disk whole disk
		    ...
		 240 = /dev/sdfd         160th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 130 char 	(Misc devices)

 130 block       SCSI disk devices (160-175)
		   0 = /dev/sdfe         161st SCSI disk whole disk
		  16 = /dev/sdff         162nd SCSI disk whole disk
		  32 = /dev/sdfg         163rd SCSI disk whole disk
		    ...
		 240 = /dev/sdft         176th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 131 block       SCSI disk devices (176-191)
		   0 = /dev/sdfu         177th SCSI disk whole disk
		  16 = /dev/sdfv         178th SCSI disk whole disk
		  32 = /dev/sdfw         179th SCSI disk whole disk
		    ...
		 240 = /dev/sdgj         192nd SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 132 block       SCSI disk devices (192-207)
		   0 = /dev/sdgk         193rd SCSI disk whole disk
		  16 = /dev/sdgl         194th SCSI disk whole disk
		  32 = /dev/sdgm         195th SCSI disk whole disk
		    ...
		 240 = /dev/sdgz         208th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 133 block       SCSI disk devices (208-223)
		   0 = /dev/sdha         209th SCSI disk whole disk
		  16 = /dev/sdhb         210th SCSI disk whole disk
		  32 = /dev/sdhc         211th SCSI disk whole disk
		    ...
		 240 = /dev/sdhp         224th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 134 block       SCSI disk devices (224-239)
		   0 = /dev/sdhq         225th SCSI disk whole disk
		  16 = /dev/sdhr         226th SCSI disk whole disk
		  32 = /dev/sdhs         227th SCSI disk whole disk
		    ...
		 240 = /dev/sdif         240th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 135 block       SCSI disk devices (240-255)
		   0 = /dev/sdig         241st SCSI disk whole disk
		  16 = /dev/sdih         242nd SCSI disk whole disk
		  32 = /dev/sdih         243rd SCSI disk whole disk
		    ...
		 240 = /dev/sdiv         256th SCSI disk whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 136-143 char	Unix98 PTY slaves
		  0 = /dev/pts/0	First Unix98 pseudo-TTY
		  1 = /dev/pts/1	Second Unix98 pseudo-TTY
		    ...

		These device nodes are automatically generated with
		the proper permissions and modes by mounting the
		devpts filesystem onto /dev/pts with the appropriate
		mount options (distribution dependent, however, on
		*most* distributions the appropriate options are
		"mode=0620,gid=<gid of the "tty" group>".)

 136 block	Mylex DAC960 PCI RAID controller; ninth controller
		  0 = /dev/rd/c8d0	First disk, whole disk
		  8 = /dev/rd/c8d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c8d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 137 block	Mylex DAC960 PCI RAID controller; tenth controller
		  0 = /dev/rd/c9d0	First disk, whole disk
		  8 = /dev/rd/c9d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c9d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 138 block	Mylex DAC960 PCI RAID controller; eleventh controller
		  0 = /dev/rd/c10d0	First disk, whole disk
		  8 = /dev/rd/c10d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c10d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 139 block	Mylex DAC960 PCI RAID controller; twelfth controller
		  0 = /dev/rd/c11d0	First disk, whole disk
		  8 = /dev/rd/c11d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c11d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 140 block	Mylex DAC960 PCI RAID controller; thirteenth controller
		  0 = /dev/rd/c12d0	First disk, whole disk
		  8 = /dev/rd/c12d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c12d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 141 block	Mylex DAC960 PCI RAID controller; fourteenth controller
		  0 = /dev/rd/c13d0	First disk, whole disk
		  8 = /dev/rd/c13d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c13d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 142 block	Mylex DAC960 PCI RAID controller; fifteenth controller
		  0 = /dev/rd/c14d0	First disk, whole disk
		  8 = /dev/rd/c14d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c14d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 143 block	Mylex DAC960 PCI RAID controller; sixteenth controller
		  0 = /dev/rd/c15d0	First disk, whole disk
		  8 = /dev/rd/c15d1	Second disk, whole disk
		    ...
		248 = /dev/rd/c15d31	32nd disk, whole disk

		Partitions are handled as for major 48.

 144 char	Encapsulated PPP
		  0 = /dev/pppox0	First PPP over Ethernet
		    ...
		 63 = /dev/pppox63	64th PPP over Ethernet

		This is primarily used for ADSL.

		The SST 5136-DN DeviceNet interface driver has been
		relocated to major 183 due to an unfortunate conflict.

 144 block	Expansion Area #1 for more non-device (e.g. NFS) mounts
		  0 = mounted device 256
		255 = mounted device 511

 145 char	SAM9407-based soundcard
		  0 = /dev/sam0_mixer
		  1 = /dev/sam0_sequencer
		  2 = /dev/sam0_midi00
		  3 = /dev/sam0_dsp
		  4 = /dev/sam0_audio
		  6 = /dev/sam0_sndstat
		 18 = /dev/sam0_midi01
		 34 = /dev/sam0_midi02
		 50 = /dev/sam0_midi03
		 64 = /dev/sam1_mixer
		    ...
		128 = /dev/sam2_mixer
		    ...
		192 = /dev/sam3_mixer
		    ...

		Device functions match OSS, but offer a number of
		addons, which are sam9407 specific.  OSS can be
		operated simultaneously, taking care of the codec.

 145 block	Expansion Area #2 for more non-device (e.g. NFS) mounts
		  0 = mounted device 512
		255 = mounted device 767

 146 char	SYSTRAM SCRAMNet mirrored-memory network
		  0 = /dev/scramnet0	First SCRAMNet device
		  1 = /dev/scramnet1	Second SCRAMNet device
		    ...

 146 block	Expansion Area #3 for more non-device (e.g. NFS) mounts
		  0 = mounted device 768
		255 = mounted device 1023

 147 char	Aureal Semiconductor Vortex Audio device
		  0 = /dev/aureal0	First Aureal Vortex
		  1 = /dev/aureal1	Second Aureal Vortex
		    ...

 147 block	Distributed Replicated Block Device (DRBD)
		  0 = /dev/drbd0	First DRBD device
		  1 = /dev/drbd1	Second DRBD device
		    ...

 148 char	Technology Concepts serial card
		  0 = /dev/ttyT0	First TCL port
		  1 = /dev/ttyT1	Second TCL port
		    ...

 149 char	Technology Concepts serial card - alternate devices
		  0 = /dev/cut0		Callout device for ttyT0
		  1 = /dev/cut0		Callout device for ttyT1
		    ...

 150 char	Real-Time Linux FIFOs
		  0 = /dev/rtf0		First RTLinux FIFO
		  1 = /dev/rtf1		Second RTLinux FIFO
		    ...

 151 char	DPT I2O SmartRaid V controller
		  0 = /dev/dpti0	First DPT I2O adapter
		  1 = /dev/dpti1	Second DPT I2O adapter
		    ...

 152 char	EtherDrive Control Device
		  0 = /dev/etherd/ctl	Connect/Disconnect an EtherDrive
		  1 = /dev/etherd/err	Monitor errors
		  2 = /dev/etherd/raw	Raw AoE packet monitor

 152 block	EtherDrive Block Devices
		  0 = /dev/etherd/0	EtherDrive 0
		    ...
		255 = /dev/etherd/255	EtherDrive 255

 153 char	SPI Bus Interface (sometimes referred to as MicroWire)
		  0 = /dev/spi0		First SPI device on the bus
		  1 = /dev/spi1		Second SPI device on the bus
		    ...
		 15 = /dev/spi15	Sixteenth SPI device on the bus

 153 block	Enhanced Metadisk RAID (EMD) storage units
		  0 = /dev/emd/0	First unit
		  1 = /dev/emd/0p1	Partition 1 on First unit
		  2 = /dev/emd/0p2	Partition 2 on First unit
		    ...
		 15 = /dev/emd/0p15	Partition 15 on First unit

		 16 = /dev/emd/1	Second unit
		 32 = /dev/emd/2	Third unit
		    ...
		240 = /dev/emd/15	Sixteenth unit

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 154 char	Specialix RIO serial card
		  0 = /dev/ttySR0	First RIO port
		    ...
		255 = /dev/ttySR255	256th RIO port

 155 char	Specialix RIO serial card - alternate devices
		  0 = /dev/cusr0	Callout device for ttySR0
		    ...
		255 = /dev/cusr255	Callout device for ttySR255

 156 char	Specialix RIO serial card
		  0 = /dev/ttySR256	257th RIO port
		    ...
		255 = /dev/ttySR511	512th RIO port

 157 char	Specialix RIO serial card - alternate devices
		  0 = /dev/cusr256	Callout device for ttySR256
		    ...
		255 = /dev/cusr511	Callout device for ttySR511

 158 char	Dialogic GammaLink fax driver
		  0 = /dev/gfax0	GammaLink channel 0
		  1 = /dev/gfax1	GammaLink channel 1
		    ...

 159 char	RESERVED

 159 block	RESERVED

 160 char	General Purpose Instrument Bus (GPIB)
		  0 = /dev/gpib0	First GPIB bus
		  1 = /dev/gpib1	Second GPIB bus
		    ...

 160 block       Carmel 8-port SATA Disks on First Controller
		  0 = /dev/carmel/0     SATA disk 0 whole disk
		  1 = /dev/carmel/0p1   SATA disk 0 partition 1
		    ...
		 31 = /dev/carmel/0p31  SATA disk 0 partition 31

		 32 = /dev/carmel/1     SATA disk 1 whole disk
		 64 = /dev/carmel/2     SATA disk 2 whole disk
		    ...
		224 = /dev/carmel/7     SATA disk 7 whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 31.

 161 char	IrCOMM devices (IrDA serial/parallel emulation)
		  0 = /dev/ircomm0	First IrCOMM device
		  1 = /dev/ircomm1	Second IrCOMM device
		    ...
		 16 = /dev/irlpt0	First IrLPT device
		 17 = /dev/irlpt1	Second IrLPT device
		    ...

 161 block       Carmel 8-port SATA Disks on Second Controller
		  0 = /dev/carmel/8     SATA disk 8 whole disk
		  1 = /dev/carmel/8p1   SATA disk 8 partition 1
		    ...
		 31 = /dev/carmel/8p31  SATA disk 8 partition 31

		 32 = /dev/carmel/9     SATA disk 9 whole disk
		 64 = /dev/carmel/10    SATA disk 10 whole disk
		    ...
		224 = /dev/carmel/15    SATA disk 15 whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 31.

 162 char	Used for (now removed) raw block device interface

 163 char

 164 char	Chase Research AT/PCI-Fast serial card
		  0 = /dev/ttyCH0	AT/PCI-Fast board 0, port 0
		    ...
		 15 = /dev/ttyCH15	AT/PCI-Fast board 0, port 15
		 16 = /dev/ttyCH16	AT/PCI-Fast board 1, port 0
		    ...
		 31 = /dev/ttyCH31	AT/PCI-Fast board 1, port 15
		 32 = /dev/ttyCH32	AT/PCI-Fast board 2, port 0
		    ...
		 47 = /dev/ttyCH47	AT/PCI-Fast board 2, port 15
		 48 = /dev/ttyCH48	AT/PCI-Fast board 3, port 0
		    ...
		 63 = /dev/ttyCH63	AT/PCI-Fast board 3, port 15

 165 char	Chase Research AT/PCI-Fast serial card - alternate devices
		  0 = /dev/cuch0	Callout device for ttyCH0
		    ...
		 63 = /dev/cuch63	Callout device for ttyCH63

 166 char	ACM USB modems
		  0 = /dev/ttyACM0	First ACM modem
		  1 = /dev/ttyACM1	Second ACM modem
		    ...

 167 char	ACM USB modems - alternate devices
		  0 = /dev/cuacm0	Callout device for ttyACM0
		  1 = /dev/cuacm1	Callout device for ttyACM1
		    ...

 168 char	Eracom CSA7000 PCI encryption adaptor
		  0 = /dev/ecsa0	First CSA7000
		  1 = /dev/ecsa1	Second CSA7000
		    ...

 169 char	Eracom CSA8000 PCI encryption adaptor
		  0 = /dev/ecsa8-0	First CSA8000
		  1 = /dev/ecsa8-1	Second CSA8000
		    ...

 170 char	AMI MegaRAC remote access controller
		  0 = /dev/megarac0	First MegaRAC card
		  1 = /dev/megarac1	Second MegaRAC card
		    ...

 171 char	Reserved for IEEE 1394 (Firewire)

 172 char	Moxa Intellio serial card
		  0 = /dev/ttyMX0	First Moxa port
		  1 = /dev/ttyMX1	Second Moxa port
		    ...
		127 = /dev/ttyMX127	128th Moxa port
		128 = /dev/moxactl	Moxa control port

 173 char	Moxa Intellio serial card - alternate devices
		  0 = /dev/cumx0	Callout device for ttyMX0
		  1 = /dev/cumx1	Callout device for ttyMX1
		    ...
		127 = /dev/cumx127	Callout device for ttyMX127

 174 char	SmartIO serial card
		  0 = /dev/ttySI0	First SmartIO port
		  1 = /dev/ttySI1	Second SmartIO port
		    ...

 175 char	SmartIO serial card - alternate devices
		  0 = /dev/cusi0	Callout device for ttySI0
		  1 = /dev/cusi1	Callout device for ttySI1
		    ...

 176 char	nCipher nFast PCI crypto accelerator
		  0 = /dev/nfastpci0	First nFast PCI device
		  1 = /dev/nfastpci1	First nFast PCI device
		    ...

 177 char	TI PCILynx memory spaces
		  0 = /dev/pcilynx/aux0	 AUX space of first PCILynx card
		    ...
		 15 = /dev/pcilynx/aux15 AUX space of 16th PCILynx card
		 16 = /dev/pcilynx/rom0	 ROM space of first PCILynx card
		    ...
		 31 = /dev/pcilynx/rom15 ROM space of 16th PCILynx card
		 32 = /dev/pcilynx/ram0	 RAM space of first PCILynx card
		    ...
		 47 = /dev/pcilynx/ram15 RAM space of 16th PCILynx card

 178 char	Giganet cLAN1xxx virtual interface adapter
		  0 = /dev/clanvi0	First cLAN adapter
		  1 = /dev/clanvi1	Second cLAN adapter
		    ...

 179 block       MMC block devices
		  0 = /dev/mmcblk0      First SD/MMC card
		  1 = /dev/mmcblk0p1    First partition on first MMC card
		  8 = /dev/mmcblk1      Second SD/MMC card
		    ...

		The start of next SD/MMC card can be configured with
		CONFIG_MMC_BLOCK_MINORS, or overridden at boot/modprobe
		time using the mmcblk.perdev_minors option. That would
		bump the offset between each card to be the configured
		value instead of the default 8.

 179 char	CCube DVXChip-based PCI products
		  0 = /dev/dvxirq0	First DVX device
		  1 = /dev/dvxirq1	Second DVX device
		    ...

 180 char	USB devices
		  0 = /dev/usb/lp0	First USB printer
		    ...
		 15 = /dev/usb/lp15	16th USB printer
		 48 = /dev/usb/scanner0	First USB scanner
		    ...
		 63 = /dev/usb/scanner15 16th USB scanner
		 64 = /dev/usb/rio500	Diamond Rio 500
		 65 = /dev/usb/usblcd	USBLCD Interface (info@usblcd.de)
		 66 = /dev/usb/cpad0	Synaptics cPad (mouse/LCD)
		 96 = /dev/usb/hiddev0	1st USB HID device
		    ...
		111 = /dev/usb/hiddev15	16th USB HID device
		112 = /dev/usb/auer0	1st auerswald ISDN device
		    ...
		127 = /dev/usb/auer15	16th auerswald ISDN device
		128 = /dev/usb/brlvgr0	First Braille Voyager device
		    ...
		131 = /dev/usb/brlvgr3	Fourth Braille Voyager device
		132 = /dev/usb/idmouse	ID Mouse (fingerprint scanner) device
		133 = /dev/usb/sisusbvga1	First SiSUSB VGA device
		    ...
		140 = /dev/usb/sisusbvga8	Eighth SISUSB VGA device
		144 = /dev/usb/lcd	USB LCD device
		160 = /dev/usb/legousbtower0	1st USB Legotower device
		    ...
		175 = /dev/usb/legousbtower15	16th USB Legotower device
		176 = /dev/usb/usbtmc1	First USB TMC device
		   ...
		191 = /dev/usb/usbtmc16	16th USB TMC device
		192 = /dev/usb/yurex1	First USB Yurex device
		   ...
		209 = /dev/usb/yurex16	16th USB Yurex device

 180 block	USB block devices
		  0 = /dev/uba		First USB block device
		  8 = /dev/ubb		Second USB block device
		 16 = /dev/ubc		Third USB block device
		    ...

 181 char	Conrad Electronic parallel port radio clocks
		  0 = /dev/pcfclock0	First Conrad radio clock
		  1 = /dev/pcfclock1	Second Conrad radio clock
		    ...

 182 char	Picture Elements THR2 binarizer
		  0 = /dev/pethr0	First THR2 board
		  1 = /dev/pethr1	Second THR2 board
		    ...

 183 char	SST 5136-DN DeviceNet interface
		  0 = /dev/ss5136dn0	First DeviceNet interface
		  1 = /dev/ss5136dn1	Second DeviceNet interface
		    ...

		This device used to be assigned to major number 144.
		It had to be moved due to an unfortunate conflict.

 184 char	Picture Elements' video simulator/sender
		  0 = /dev/pevss0	First sender board
		  1 = /dev/pevss1	Second sender board
		    ...

 185 char	InterMezzo high availability file system
		  0 = /dev/intermezzo0	First cache manager
		  1 = /dev/intermezzo1	Second cache manager
		    ...

		See http://web.archive.org/web/20080115195241/
		http://inter-mezzo.org/index.html

 186 char	Object-based storage control device
		  0 = /dev/obd0		First obd control device
		  1 = /dev/obd1		Second obd control device
		    ...

		See ftp://ftp.lustre.org/pub/obd for code and information.

 187 char	DESkey hardware encryption device
		  0 = /dev/deskey0	First DES key
		  1 = /dev/deskey1	Second DES key
		    ...

 188 char	USB serial converters
		  0 = /dev/ttyUSB0	First USB serial converter
		  1 = /dev/ttyUSB1	Second USB serial converter
		    ...

 189 char	USB serial converters - alternate devices
		  0 = /dev/cuusb0	Callout device for ttyUSB0
		  1 = /dev/cuusb1	Callout device for ttyUSB1
		    ...

 190 char	Kansas City tracker/tuner card
		  0 = /dev/kctt0	First KCT/T card
		  1 = /dev/kctt1	Second KCT/T card
		    ...

 191 char	Reserved for PCMCIA

 192 char	Kernel profiling interface
		  0 = /dev/profile	Profiling control device
		  1 = /dev/profile0	Profiling device for CPU 0
		  2 = /dev/profile1	Profiling device for CPU 1
		    ...

 193 char	Kernel event-tracing interface
		  0 = /dev/trace	Tracing control device
		  1 = /dev/trace0	Tracing device for CPU 0
		  2 = /dev/trace1	Tracing device for CPU 1
		    ...

 194 char	linVideoStreams (LINVS)
		  0 = /dev/mvideo/status0	Video compression status
		  1 = /dev/mvideo/stream0	Video stream
		  2 = /dev/mvideo/frame0	Single compressed frame
		  3 = /dev/mvideo/rawframe0	Raw uncompressed frame
		  4 = /dev/mvideo/codec0	Direct codec access
		  5 = /dev/mvideo/video4linux0	Video4Linux compatibility

		 16 = /dev/mvideo/status1	Second device
		    ...
		 32 = /dev/mvideo/status2	Third device
		    ...
		    ...
		240 = /dev/mvideo/status15	16th device
		    ...

 195 char	Nvidia graphics devices
		  0 = /dev/nvidia0		First Nvidia card
		  1 = /dev/nvidia1		Second Nvidia card
		    ...
		255 = /dev/nvidiactl		Nvidia card control device

 196 char	Tormenta T1 card
		  0 = /dev/tor/0		Master control channel for all cards
		  1 = /dev/tor/1		First DS0
		  2 = /dev/tor/2		Second DS0
		    ...
		 48 = /dev/tor/48		48th DS0
		 49 = /dev/tor/49		First pseudo-channel
		 50 = /dev/tor/50		Second pseudo-channel
		    ...

 197 char	OpenTNF tracing facility
		  0 = /dev/tnf/t0		Trace 0 data extraction
		  1 = /dev/tnf/t1		Trace 1 data extraction
		    ...
		128 = /dev/tnf/status		Tracing facility status
		130 = /dev/tnf/trace		Tracing device

 198 char	Total Impact TPMP2 quad coprocessor PCI card
		  0 = /dev/tpmp2/0		First card
		  1 = /dev/tpmp2/1		Second card
		    ...

 199 char	Veritas volume manager (VxVM) volumes
		  0 = /dev/vx/rdsk/*/*		First volume
		  1 = /dev/vx/rdsk/*/*		Second volume
		    ...

 199 block	Veritas volume manager (VxVM) volumes
		  0 = /dev/vx/dsk/*/*		First volume
		  1 = /dev/vx/dsk/*/*		Second volume
		    ...

		The namespace in these directories is maintained by
		the user space VxVM software.

 200 char	Veritas VxVM configuration interface
		   0 = /dev/vx/config		Configuration access node
		   1 = /dev/vx/trace		Volume i/o trace access node
		   2 = /dev/vx/iod		Volume i/o daemon access node
		   3 = /dev/vx/info		Volume information access node
		   4 = /dev/vx/task		Volume tasks access node
		   5 = /dev/vx/taskmon		Volume tasks monitor daemon

 201 char	Veritas VxVM dynamic multipathing driver
		  0 = /dev/vx/rdmp/*		First multipath device
		  1 = /dev/vx/rdmp/*		Second multipath device
		    ...
 201 block	Veritas VxVM dynamic multipathing driver
		  0 = /dev/vx/dmp/*		First multipath device
		  1 = /dev/vx/dmp/*		Second multipath device
		    ...

		The namespace in these directories is maintained by
		the user space VxVM software.

 202 char	CPU model-specific registers
		  0 = /dev/cpu/0/msr		MSRs on CPU 0
		  1 = /dev/cpu/1/msr		MSRs on CPU 1
		    ...

 202 block	Xen Virtual Block Device
		  0 = /dev/xvda       First Xen VBD whole disk
		  16 = /dev/xvdb      Second Xen VBD whole disk
		  32 = /dev/xvdc      Third Xen VBD whole disk
		    ...
		  240 = /dev/xvdp     Sixteenth Xen VBD whole disk

		Partitions are handled in the same way as for IDE
		disks (see major number 3) except that the limit on
		partitions is 15.

 203 char	CPU CPUID information
		  0 = /dev/cpu/0/cpuid		CPUID on CPU 0
		  1 = /dev/cpu/1/cpuid		CPUID on CPU 1
		    ...

 204 char	Low-density serial ports
		  0 = /dev/ttyLU0		LinkUp Systems L72xx UART - port 0
		  1 = /dev/ttyLU1		LinkUp Systems L72xx UART - port 1
		  2 = /dev/ttyLU2		LinkUp Systems L72xx UART - port 2
		  3 = /dev/ttyLU3		LinkUp Systems L72xx UART - port 3
		  4 = /dev/ttyFB0		Intel Footbridge (ARM)
		  5 = /dev/ttySA0		StrongARM builtin serial port 0
		  6 = /dev/ttySA1		StrongARM builtin serial port 1
		  7 = /dev/ttySA2		StrongARM builtin serial port 2
		  8 = /dev/ttySC0		SCI serial port (SuperH) - port 0
		  9 = /dev/ttySC1		SCI serial port (SuperH) - port 1
		 10 = /dev/ttySC2		SCI serial port (SuperH) - port 2
		 11 = /dev/ttySC3		SCI serial port (SuperH) - port 3
		 12 = /dev/ttyFW0		Firmware console - port 0
		 13 = /dev/ttyFW1		Firmware console - port 1
		 14 = /dev/ttyFW2		Firmware console - port 2
		 15 = /dev/ttyFW3		Firmware console - port 3
		 16 = /dev/ttyAM0		ARM "AMBA" serial port 0
		    ...
		 31 = /dev/ttyAM15		ARM "AMBA" serial port 15
		 32 = /dev/ttyDB0		DataBooster serial port 0
		    ...
		 39 = /dev/ttyDB7		DataBooster serial port 7
		 40 = /dev/ttySG0		SGI Altix console port
		 41 = /dev/ttySMX0		Motorola i.MX - port 0
		 42 = /dev/ttySMX1		Motorola i.MX - port 1
		 43 = /dev/ttySMX2		Motorola i.MX - port 2
		 44 = /dev/ttyMM0		Marvell MPSC - port 0 (obsolete unused)
		 45 = /dev/ttyMM1		Marvell MPSC - port 1 (obsolete unused)
		 46 = /dev/ttyCPM0		PPC CPM (SCC or SMC) - port 0
		    ...
		 51 = /dev/ttyCPM5		PPC CPM (SCC or SMC) - port 5
		 82 = /dev/ttyVR0		NEC VR4100 series SIU
		 83 = /dev/ttyVR1		NEC VR4100 series DSIU
		 148 = /dev/ttyPSC0		PPC PSC - port 0
		    ...
		 153 = /dev/ttyPSC5		PPC PSC - port 5
		 154 = /dev/ttyAT0		ATMEL serial port 0
		    ...
		 169 = /dev/ttyAT15		ATMEL serial port 15
		 170 = /dev/ttyNX0		Hilscher netX serial port 0
		    ...
		 185 = /dev/ttyNX15		Hilscher netX serial port 15
		 186 = /dev/ttyJ0		JTAG1 DCC protocol based serial port emulation

		 If maximum number of uartlite serial ports is more than 4, then the driver
		 uses dynamic allocation instead of static allocation for major number.
		 187 = /dev/ttyUL0		Xilinx uartlite - port 0
		    ...
		 190 = /dev/ttyUL3		Xilinx uartlite - port 3
		 191 = /dev/xvc0		Xen virtual console - port 0
		 192 = /dev/ttyPZ0		pmac_zilog - port 0
		    ...
		 195 = /dev/ttyPZ3		pmac_zilog - port 3
		 196 = /dev/ttyTX0		TX39/49 serial port 0
		    ...
		 204 = /dev/ttyTX7		TX39/49 serial port 7
		 205 = /dev/ttySC0		SC26xx serial port 0
		 206 = /dev/ttySC1		SC26xx serial port 1
		 207 = /dev/ttySC2		SC26xx serial port 2
		 208 = /dev/ttySC3		SC26xx serial port 3
		 209 = /dev/ttyMAX0		MAX3100 serial port 0
		 210 = /dev/ttyMAX1		MAX3100 serial port 1
		 211 = /dev/ttyMAX2		MAX3100 serial port 2
		 212 = /dev/ttyMAX3		MAX3100 serial port 3

 205 char	Low-density serial ports (alternate device)
		  0 = /dev/culu0		Callout device for ttyLU0
		  1 = /dev/culu1		Callout device for ttyLU1
		  2 = /dev/culu2		Callout device for ttyLU2
		  3 = /dev/culu3		Callout device for ttyLU3
		  4 = /dev/cufb0		Callout device for ttyFB0
		  5 = /dev/cusa0		Callout device for ttySA0
		  6 = /dev/cusa1		Callout device for ttySA1
		  7 = /dev/cusa2		Callout device for ttySA2
		  8 = /dev/cusc0		Callout device for ttySC0
		  9 = /dev/cusc1		Callout device for ttySC1
		 10 = /dev/cusc2		Callout device for ttySC2
		 11 = /dev/cusc3		Callout device for ttySC3
		 12 = /dev/cufw0		Callout device for ttyFW0
		 13 = /dev/cufw1		Callout device for ttyFW1
		 14 = /dev/cufw2		Callout device for ttyFW2
		 15 = /dev/cufw3		Callout device for ttyFW3
		 16 = /dev/cuam0		Callout device for ttyAM0
		    ...
		 31 = /dev/cuam15		Callout device for ttyAM15
		 32 = /dev/cudb0		Callout device for ttyDB0
		    ...
		 39 = /dev/cudb7		Callout device for ttyDB7
		 40 = /dev/cusg0		Callout device for ttySG0
		 41 = /dev/ttycusmx0		Callout device for ttySMX0
		 42 = /dev/ttycusmx1		Callout device for ttySMX1
		 43 = /dev/ttycusmx2		Callout device for ttySMX2
		 46 = /dev/cucpm0		Callout device for ttyCPM0
		    ...
		 51 = /dev/cucpm5		Callout device for ttyCPM5
		 82 = /dev/cuvr0		Callout device for ttyVR0
		 83 = /dev/cuvr1		Callout device for ttyVR1

 206 char	OnStream SC-x0 tape devices
		  0 = /dev/osst0		First OnStream SCSI tape, mode 0
		  1 = /dev/osst1		Second OnStream SCSI tape, mode 0
		    ...
		 32 = /dev/osst0l		First OnStream SCSI tape, mode 1
		 33 = /dev/osst1l		Second OnStream SCSI tape, mode 1
		    ...
		 64 = /dev/osst0m		First OnStream SCSI tape, mode 2
		 65 = /dev/osst1m		Second OnStream SCSI tape, mode 2
		    ...
		 96 = /dev/osst0a		First OnStream SCSI tape, mode 3
		 97 = /dev/osst1a		Second OnStream SCSI tape, mode 3
		    ...
		128 = /dev/nosst0		No rewind version of /dev/osst0
		129 = /dev/nosst1		No rewind version of /dev/osst1
		    ...
		160 = /dev/nosst0l		No rewind version of /dev/osst0l
		161 = /dev/nosst1l		No rewind version of /dev/osst1l
		    ...
		192 = /dev/nosst0m		No rewind version of /dev/osst0m
		193 = /dev/nosst1m		No rewind version of /dev/osst1m
		    ...
		224 = /dev/nosst0a		No rewind version of /dev/osst0a
		225 = /dev/nosst1a		No rewind version of /dev/osst1a
		    ...

		The OnStream SC-x0 SCSI tapes do not support the
		standard SCSI SASD command set and therefore need
		their own driver "osst". Note that the IDE, USB (and
		maybe ParPort) versions may be driven via ide-scsi or
		usb-storage SCSI emulation and this osst device and
		driver as well.  The ADR-x0 drives are QIC-157
		compliant and don't need osst.

 207 char	Compaq ProLiant health feature indicate
		  0 = /dev/cpqhealth/cpqw	Redirector interface
		  1 = /dev/cpqhealth/crom	EISA CROM
		  2 = /dev/cpqhealth/cdt	Data Table
		  3 = /dev/cpqhealth/cevt	Event Log
		  4 = /dev/cpqhealth/casr	Automatic Server Recovery
		  5 = /dev/cpqhealth/cecc	ECC Memory
		  6 = /dev/cpqhealth/cmca	Machine Check Architecture
		  7 = /dev/cpqhealth/ccsm	Deprecated CDT
		  8 = /dev/cpqhealth/cnmi	NMI Handling
		  9 = /dev/cpqhealth/css	Sideshow Management
		 10 = /dev/cpqhealth/cram	CMOS interface
		 11 = /dev/cpqhealth/cpci	PCI IRQ interface

 208 char	User space serial ports
		  0 = /dev/ttyU0		First user space serial port
		  1 = /dev/ttyU1		Second user space serial port
		    ...

 209 char	User space serial ports (alternate devices)
		  0 = /dev/cuu0			Callout device for ttyU0
		  1 = /dev/cuu1			Callout device for ttyU1
		    ...

 210 char	SBE, Inc. sync/async serial card
		  0 = /dev/sbei/wxcfg0		Configuration device for board 0
		  1 = /dev/sbei/dld0		Download device for board 0
		  2 = /dev/sbei/wan00		WAN device, port 0, board 0
		  3 = /dev/sbei/wan01		WAN device, port 1, board 0
		  4 = /dev/sbei/wan02		WAN device, port 2, board 0
		  5 = /dev/sbei/wan03		WAN device, port 3, board 0
		  6 = /dev/sbei/wanc00		WAN clone device, port 0, board 0
		  7 = /dev/sbei/wanc01		WAN clone device, port 1, board 0
		  8 = /dev/sbei/wanc02		WAN clone device, port 2, board 0
		  9 = /dev/sbei/wanc03		WAN clone device, port 3, board 0
		 10 = /dev/sbei/wxcfg1		Configuration device for board 1
		 11 = /dev/sbei/dld1		Download device for board 1
		 12 = /dev/sbei/wan10		WAN device, port 0, board 1
		 13 = /dev/sbei/wan11		WAN device, port 1, board 1
		 14 = /dev/sbei/wan12		WAN device, port 2, board 1
		 15 = /dev/sbei/wan13		WAN device, port 3, board 1
		 16 = /dev/sbei/wanc10		WAN clone device, port 0, board 1
		 17 = /dev/sbei/wanc11		WAN clone device, port 1, board 1
		 18 = /dev/sbei/wanc12		WAN clone device, port 2, board 1
		 19 = /dev/sbei/wanc13		WAN clone device, port 3, board 1
		    ...

		Yes, each board is really spaced 10 (decimal) apart.

 211 char	Addinum CPCI1500 digital I/O card
		  0 = /dev/addinum/cpci1500/0	First CPCI1500 card
		  1 = /dev/addinum/cpci1500/1	Second CPCI1500 card
		    ...

 212 char	LinuxTV.org DVB driver subsystem
		  0 = /dev/dvb/adapter0/video0    first video decoder of first card
		  1 = /dev/dvb/adapter0/audio0    first audio decoder of first card
		  2 = /dev/dvb/adapter0/sec0      (obsolete/unused)
		  3 = /dev/dvb/adapter0/frontend0 first frontend device of first card
		  4 = /dev/dvb/adapter0/demux0    first demux device of first card
		  5 = /dev/dvb/adapter0/dvr0      first digital video recoder device of first card
		  6 = /dev/dvb/adapter0/ca0       first common access port of first card
		  7 = /dev/dvb/adapter0/net0      first network device of first card
		  8 = /dev/dvb/adapter0/osd0      first on-screen-display device of first card
		  9 = /dev/dvb/adapter0/video1    second video decoder of first card
		    ...
		 64 = /dev/dvb/adapter1/video0    first video decoder of second card
		    ...
		128 = /dev/dvb/adapter2/video0    first video decoder of third card
		    ...
		196 = /dev/dvb/adapter3/video0    first video decoder of fourth card

 216 char	Bluetooth RFCOMM TTY devices
		  0 = /dev/rfcomm0		First Bluetooth RFCOMM TTY device
		  1 = /dev/rfcomm1		Second Bluetooth RFCOMM TTY device
		    ...

 217 char	Bluetooth RFCOMM TTY devices (alternate devices)
		  0 = /dev/curf0		Callout device for rfcomm0
		  1 = /dev/curf1		Callout device for rfcomm1
		    ...

 218 char	The Logical Company bus Unibus/Qbus adapters
		  0 = /dev/logicalco/bci/0	First bus adapter
		  1 = /dev/logicalco/bci/1	First bus adapter
		    ...

 219 char	The Logical Company DCI-1300 digital I/O card
		  0 = /dev/logicalco/dci1300/0	First DCI-1300 card
		  1 = /dev/logicalco/dci1300/1	Second DCI-1300 card
		    ...

 220 char	Myricom Myrinet "GM" board
		  0 = /dev/myricom/gm0		First Myrinet GM board
		  1 = /dev/myricom/gmp0		First board "root access"
		  2 = /dev/myricom/gm1		Second Myrinet GM board
		  3 = /dev/myricom/gmp1		Second board "root access"
		    ...

 221 char	VME bus
		  0 = /dev/bus/vme/m0		First master image
		  1 = /dev/bus/vme/m1		Second master image
		  2 = /dev/bus/vme/m2		Third master image
		  3 = /dev/bus/vme/m3		Fourth master image
		  4 = /dev/bus/vme/s0		First slave image
		  5 = /dev/bus/vme/s1		Second slave image
		  6 = /dev/bus/vme/s2		Third slave image
		  7 = /dev/bus/vme/s3		Fourth slave image
		  8 = /dev/bus/vme/ctl		Control

		It is expected that all VME bus drivers will use the
		same interface.  For interface documentation see
		http://www.vmelinux.org/.

 224 char	A2232 serial card
		  0 = /dev/ttyY0		First A2232 port
		  1 = /dev/ttyY1		Second A2232 port
		    ...

 225 char	A2232 serial card (alternate devices)
		  0 = /dev/cuy0			Callout device for ttyY0
		  1 = /dev/cuy1			Callout device for ttyY1
		    ...

 226 char	Direct Rendering Infrastructure (DRI)
		  0 = /dev/dri/card0		First graphics card
		  1 = /dev/dri/card1		Second graphics card
		    ...

 227 char	IBM 3270 terminal Unix tty access
		  1 = /dev/3270/tty1		First 3270 terminal
		  2 = /dev/3270/tty2		Seconds 3270 terminal
		    ...

 228 char	IBM 3270 terminal block-mode access
		  0 = /dev/3270/tub		Controlling interface
		  1 = /dev/3270/tub1		First 3270 terminal
		  2 = /dev/3270/tub2		Second 3270 terminal
		    ...

 229 char	IBM iSeries/pSeries virtual console
		  0 = /dev/hvc0			First console port
		  1 = /dev/hvc1			Second console port
		    ...

 230 char	IBM iSeries virtual tape
		  0 = /dev/iseries/vt0		First virtual tape, mode 0
		  1 = /dev/iseries/vt1		Second virtual tape, mode 0
		    ...
		 32 = /dev/iseries/vt0l		First virtual tape, mode 1
		 33 = /dev/iseries/vt1l		Second virtual tape, mode 1
		    ...
		 64 = /dev/iseries/vt0m		First virtual tape, mode 2
		 65 = /dev/iseries/vt1m		Second virtual tape, mode 2
		    ...
		 96 = /dev/iseries/vt0a		First virtual tape, mode 3
		 97 = /dev/iseries/vt1a		Second virtual tape, mode 3
		      ...
		128 = /dev/iseries/nvt0		First virtual tape, mode 0, no rewind
		129 = /dev/iseries/nvt1		Second virtual tape, mode 0, no rewind
		    ...
		160 = /dev/iseries/nvt0l	First virtual tape, mode 1, no rewind
		161 = /dev/iseries/nvt1l	Second virtual tape, mode 1, no rewind
		    ...
		192 = /dev/iseries/nvt0m	First virtual tape, mode 2, no rewind
		193 = /dev/iseries/nvt1m	Second virtual tape, mode 2, no rewind
		    ...
		224 = /dev/iseries/nvt0a	First virtual tape, mode 3, no rewind
		225 = /dev/iseries/nvt1a	Second virtual tape, mode 3, no rewind
		    ...

		"No rewind" refers to the omission of the default
		automatic rewind on device close.  The MTREW or MTOFFL
		ioctl()'s can be used to rewind the tape regardless of
		the device used to access it.

 231 char	InfiniBand
		0 = /dev/infiniband/umad0
		1 = /dev/infiniband/umad1
		  ...
		63 = /dev/infiniband/umad63    63rd InfiniBandMad device
		64 = /dev/infiniband/issm0     First InfiniBand IsSM device
		65 = /dev/infiniband/issm1     Second InfiniBand IsSM device
		  ...
		127 = /dev/infiniband/issm63    63rd InfiniBand IsSM device
		192 = /dev/infiniband/uverbs0   First InfiniBand verbs device
		193 = /dev/infiniband/uverbs1   Second InfiniBand verbs device
		  ...
		223 = /dev/infiniband/uverbs31  31st InfiniBand verbs device

 232 char	Biometric Devices
		0 = /dev/biometric/sensor0/fingerprint	first fingerprint sensor on first device
		1 = /dev/biometric/sensor0/iris		first iris sensor on first device
		2 = /dev/biometric/sensor0/retina	first retina sensor on first device
		3 = /dev/biometric/sensor0/voiceprint	first voiceprint sensor on first device
		4 = /dev/biometric/sensor0/facial	first facial sensor on first device
		5 = /dev/biometric/sensor0/hand		first hand sensor on first device
		  ...
		10 = /dev/biometric/sensor1/fingerprint	first fingerprint sensor on second device
		  ...
		20 = /dev/biometric/sensor2/fingerprint	first fingerprint sensor on third device
		  ...

 233 char	PathScale InfiniPath interconnect
		0 = /dev/ipath        Primary device for programs (any unit)
		1 = /dev/ipath0       Access specifically to unit 0
		2 = /dev/ipath1       Access specifically to unit 1
		  ...
		4 = /dev/ipath3       Access specifically to unit 3
		129 = /dev/ipath_sma    Device used by Subnet Management Agent
		130 = /dev/ipath_diag   Device used by diagnostics programs

 234-254	char	RESERVED FOR DYNAMIC ASSIGNMENT
		Character devices that request a dynamic allocation of major number will
		take numbers starting from 254 and downward.

 240-254 block	LOCAL/EXPERIMENTAL USE
		Allocated for local/experimental use.  For devices not
		assigned official numbers, these ranges should be
		used in order to avoid conflicting with future assignments.

 255 char	RESERVED

 255 block	RESERVED

		This major is reserved to assist the expansion to a
		larger number space.  No device nodes with this major
		should ever be created on the filesystem.
		(This is probably not true anymore, but I'll leave it
		for now /Torben)

 ---LARGE MAJORS!!!!!---

 256 char	Equinox SST multi-port serial boards
		   0 = /dev/ttyEQ0	First serial port on first Equinox SST board
		 127 = /dev/ttyEQ127	Last serial port on first Equinox SST board
		 128 = /dev/ttyEQ128	First serial port on second Equinox SST board
		  ...
		1027 = /dev/ttyEQ1027	Last serial port on eighth Equinox SST board

 256 block	Resident Flash Disk Flash Translation Layer
		  0 = /dev/rfda		First RFD FTL layer
		 16 = /dev/rfdb		Second RFD FTL layer
		  ...
		240 = /dev/rfdp		16th RFD FTL layer

 257 char	Phoenix Technologies Cryptographic Services Driver
		  0 = /dev/ptlsec	Crypto Services Driver

 257 block	SSFDC Flash Translation Layer filesystem
		  0 = /dev/ssfdca	First SSFDC layer
		  8 = /dev/ssfdcb	Second SSFDC layer
		 16 = /dev/ssfdcc	Third SSFDC layer
		 24 = /dev/ssfdcd	4th SSFDC layer
		 32 = /dev/ssfdce	5th SSFDC layer
		 40 = /dev/ssfdcf	6th SSFDC layer
		 48 = /dev/ssfdcg	7th SSFDC layer
		 56 = /dev/ssfdch	8th SSFDC layer

 258 block	ROM/Flash read-only translation layer
		  0 = /dev/blockrom0	First ROM card's translation layer interface
		  1 = /dev/blockrom1	Second ROM card's translation layer interface
		  ...

 259 block	Block Extended Major
		  Used dynamically to hold additional partition minor
		  numbers and allow large numbers of partitions per device

 259 char	FPGA configuration interfaces
		  0 = /dev/icap0	First Xilinx internal configuration
		  1 = /dev/icap1	Second Xilinx internal configuration

 260 char	OSD (Object-based-device) SCSI Device
		  0 = /dev/osd0		First OSD Device
		  1 = /dev/osd1		Second OSD Device
		  ...
		  255 = /dev/osd255	256th OSD Device

 261 char	Compute Acceleration Devices
		  0 = /dev/accel/accel0	First acceleration device
		  1 = /dev/accel/accel1	Second acceleration device
		    ...

 384-511 char	RESERVED FOR DYNAMIC ASSIGNMENT
		Character devices that request a dynamic allocation of major
		number will take numbers starting from 511 and downward,
		once the 234-254 range is full.
--
=============
Ioctl Numbers
=============

19 October 1999

Michael Elizabeth Chastain
<mec@shout.net>

If you are adding new ioctl's to the kernel, you should use the _IO
macros defined in <linux/ioctl.h>:

    ====== ===========================
    macro  parameters
    ====== ===========================
    _IO    none
    _IOW   write (read from userspace)
    _IOR   read (write to userpace)
    _IOWR  write and read
    ====== ===========================

'Write' and 'read' are from the user's point of view, just like the
system calls 'write' and 'read'.  For example, a SET_FOO ioctl would
be _IOW, although the kernel would actually read data from user space;
a GET_FOO ioctl would be _IOR, although the kernel would actually write
data to user space.

The first argument to the macros is an identifying letter or number from
the table below. Because of the large number of drivers, many drivers
share a partial letter with other drivers.

If you are writing a driver for a new device and need a letter, pick an
unused block with enough room for expansion: 32 to 256 ioctl commands
should suffice. You can register the block by patching this file and
submitting the patch through :doc:`usual patch submission process
</process/submitting-patches>`.

The second argument is a sequence number to distinguish ioctls from each
other. The third argument (not applicable to _IO) is the type of the data
going into the kernel or coming out of the kernel (e.g.  'int' or
'struct foo').

.. note::
   Do NOT use sizeof(arg) as the third argument as this results in your
   ioctl thinking it passes an argument of type size_t.

Some devices use their major number as the identifier; this is OK, as
long as it is unique.  Some devices are irregular and don't follow any
convention at all.

Following this convention is good because:

(1) Keeping the ioctl's globally unique helps error checking:
    if a program calls an ioctl on the wrong device, it will get an
    error rather than some unexpected behaviour.

(2) The 'strace' build procedure automatically finds ioctl numbers
    defined with the macros.

(3) 'strace' can decode numbers back into useful names when the
    numbers are unique.

(4) People looking for ioctls can grep for them more easily when
    this convention is used to define the ioctl numbers.

(5) When following the convention, the driver code can use generic
    code to copy the parameters between user and kernel space.

This table lists ioctls visible from userland, excluding ones from
drivers/staging/.

====  =====  ========================================================= ================================================================
Code  Seq#    Include File                                             Comments
      (hex)
====  =====  ========================================================= ================================================================
0x00  00-1F  linux/fs.h                                                conflict!
0x00  00-1F  scsi/scsi_ioctl.h                                         conflict!
0x00  00-1F  linux/fb.h                                                conflict!
0x00  00-1F  linux/wavefront.h                                         conflict!
0x02  all    linux/fd.h
0x03  all    linux/hdreg.h
0x04  D2-DC  linux/umsdos_fs.h                                         Dead since 2.6.11, but don't reuse these.
0x06  all    linux/lp.h
0x07  9F-D0  linux/vmw_vmci_defs.h, uapi/linux/vm_sockets.h
0x09  all    linux/raid/md_u.h
0x10  00-0F  drivers/char/s390/vmcp.h
0x10  10-1F  arch/s390/include/uapi/sclp_ctl.h
0x10  20-2F  arch/s390/include/uapi/asm/hypfs.h
0x12  all    linux/fs.h                                                BLK* ioctls
             linux/blkpg.h
             linux/blkzoned.h
             linux/blk-crypto.h
0x15  all    linux/fs.h                                                FS_IOC_* ioctls
0x1b  all                                                              InfiniBand Subsystem
                                                                       <http://infiniband.sourceforge.net/>
0x20  all    drivers/cdrom/cm206.h
0x22  all    scsi/sg.h
0x3E  00-0F  linux/counter.h                                           <mailto:linux-iio@vger.kernel.org>
'!'   00-1F  uapi/linux/seccomp.h
'#'   00-3F                                                            IEEE 1394 Subsystem
                                                                       Block for the entire subsystem
'$'   00-0F  linux/perf_counter.h, linux/perf_event.h
'%'   00-0F  include/uapi/linux/stm.h                                  System Trace Module subsystem
                                                                       <mailto:alexander.shishkin@linux.intel.com>
'&'   00-07  drivers/firewire/nosy-user.h
'*'   00-1F  uapi/linux/user_events.h                                  User Events Subsystem
                                                                       <mailto:linux-trace-kernel@vger.kernel.org>
'1'   00-1F  linux/timepps.h                                           PPS kit from Ulrich Windl
                                                                       <ftp://ftp.de.kernel.org/pub/linux/daemons/ntp/PPS/>
'2'   01-04  linux/i2o.h
'3'   00-0F  drivers/s390/char/raw3270.h                               conflict!
'3'   00-1F  linux/suspend_ioctls.h,                                   conflict!
             kernel/power/user.c
'8'   all                                                              SNP8023 advanced NIC card
                                                                       <mailto:mcr@solidum.com>
';'   64-7F  linux/vfio.h
';'   80-FF  linux/iommufd.h
'='   00-3f  uapi/linux/ptp_clock.h                                    <mailto:richardcochran@gmail.com>
'@'   00-0F  linux/radeonfb.h                                          conflict!
'@'   00-0F  drivers/video/aty/aty128fb.c                              conflict!
'A'   00-1F  linux/apm_bios.h                                          conflict!
'A'   00-0F  linux/agpgart.h,                                          conflict!
             drivers/char/agp/compat_ioctl.h
'A'   00-7F  sound/asound.h                                            conflict!
'B'   00-1F  linux/cciss_ioctl.h                                       conflict!
'B'   00-0F  include/linux/pmu.h                                       conflict!
'B'   C0-FF  advanced bbus                                             <mailto:maassen@uni-freiburg.de>
'B'   00-0F  xen/xenbus_dev.h                                          conflict!
'C'   all    linux/soundcard.h                                         conflict!
'C'   01-2F  linux/capi.h                                              conflict!
'C'   F0-FF  drivers/net/wan/cosa.h                                    conflict!
'D'   all    arch/s390/include/asm/dasd.h
'D'   40-5F  drivers/scsi/dpt/dtpi_ioctl.h                             Dead since 2022
'D'   05     drivers/scsi/pmcraid.h
'E'   all    linux/input.h                                             conflict!
'E'   00-0F  xen/evtchn.h                                              conflict!
'F'   all    linux/fb.h                                                conflict!
'F'   01-02  drivers/scsi/pmcraid.h                                    conflict!
'F'   20     drivers/video/fsl-diu-fb.h                                conflict!
'F'   20     linux/ivtvfb.h                                            conflict!
'F'   20     linux/matroxfb.h                                          conflict!
'F'   20     drivers/video/aty/atyfb_base.c                            conflict!
'F'   00-0F  video/da8xx-fb.h                                          conflict!
'F'   80-8F  linux/arcfb.h                                             conflict!
'F'   DD     video/sstfb.h                                             conflict!
'G'   00-3F  drivers/misc/sgi-gru/grulib.h                             conflict!
'G'   00-0F  xen/gntalloc.h, xen/gntdev.h                              conflict!
'H'   00-7F  linux/hiddev.h                                            conflict!
'H'   00-0F  linux/hidraw.h                                            conflict!
'H'   01     linux/mei.h                                               conflict!
'H'   02     linux/mei.h                                               conflict!
'H'   03     linux/mei.h                                               conflict!
'H'   00-0F  sound/asound.h                                            conflict!
'H'   20-40  sound/asound_fm.h                                         conflict!
'H'   80-8F  sound/sfnt_info.h                                         conflict!
'H'   10-8F  sound/emu10k1.h                                           conflict!
'H'   10-1F  sound/sb16_csp.h                                          conflict!
'H'   10-1F  sound/hda_hwdep.h                                         conflict!
'H'   40-4F  sound/hdspm.h                                             conflict!
'H'   40-4F  sound/hdsp.h                                              conflict!
'H'   90     sound/usb/usx2y/usb_stream.h
'H'   00-0F  uapi/misc/habanalabs.h                                    conflict!
'H'   A0     uapi/linux/usb/cdc-wdm.h
'H'   C0-F0  net/bluetooth/hci.h                                       conflict!
'H'   C0-DF  net/bluetooth/hidp/hidp.h                                 conflict!
'H'   C0-DF  net/bluetooth/cmtp/cmtp.h                                 conflict!
'H'   C0-DF  net/bluetooth/bnep/bnep.h                                 conflict!
'H'   F1     linux/hid-roccat.h                                        <mailto:erazor_de@users.sourceforge.net>
'H'   F8-FA  sound/firewire.h
'I'   all    linux/isdn.h                                              conflict!
'I'   00-0F  drivers/isdn/divert/isdn_divert.h                         conflict!
'I'   40-4F  linux/mISDNif.h                                           conflict!
'K'   all    linux/kd.h
'L'   00-1F  linux/loop.h                                              conflict!
'L'   10-1F  drivers/scsi/mpt3sas/mpt3sas_ctl.h                        conflict!
'L'   E0-FF  linux/ppdd.h                                              encrypted disk device driver
                                                                       <http://linux01.gwdg.de/~alatham/ppdd.html>
'M'   all    linux/soundcard.h                                         conflict!
'M'   01-16  mtd/mtd-abi.h                                             conflict!
      and    drivers/mtd/mtdchar.c
'M'   01-03  drivers/scsi/megaraid/megaraid_sas.h
'M'   00-0F  drivers/video/fsl-diu-fb.h                                conflict!
'N'   00-1F  drivers/usb/scanner.h
'N'   40-7F  drivers/block/nvme.c
'N'   80-8F  uapi/linux/ntsync.h                                       NT synchronization primitives
                                                                       <mailto:wine-devel@winehq.org>
'O'   00-06  mtd/ubi-user.h                                            UBI
'P'   all    linux/soundcard.h                                         conflict!
'P'   60-6F  sound/sscape_ioctl.h                                      conflict!
'P'   00-0F  drivers/usb/class/usblp.c                                 conflict!
'P'   01-09  drivers/misc/pci_endpoint_test.c                          conflict!
'P'   00-0F  xen/privcmd.h                                             conflict!
'P'   00-05  linux/tps6594_pfsm.h                                      conflict!
'Q'   all    linux/soundcard.h
'R'   00-1F  linux/random.h                                            conflict!
'R'   01     linux/rfkill.h                                            conflict!
'R'   20-2F  linux/trace_mmap.h
'R'   C0-DF  net/bluetooth/rfcomm.h
'R'   E0     uapi/linux/fsl_mc.h
'S'   all    linux/cdrom.h                                             conflict!
'S'   80-81  scsi/scsi_ioctl.h                                         conflict!
'S'   82-FF  scsi/scsi.h                                               conflict!
'S'   00-7F  sound/asequencer.h                                        conflict!
'T'   all    linux/soundcard.h                                         conflict!
'T'   00-AF  sound/asound.h                                            conflict!
'T'   all    arch/x86/include/asm/ioctls.h                             conflict!
'T'   C0-DF  linux/if_tun.h                                            conflict!
'U'   all    sound/asound.h                                            conflict!
'U'   00-CF  linux/uinput.h                                            conflict!
'U'   00-EF  linux/usbdevice_fs.h
'U'   C0-CF  drivers/bluetooth/hci_uart.h
'V'   all    linux/vt.h                                                conflict!
'V'   all    linux/videodev2.h                                         conflict!
'V'   C0     linux/ivtvfb.h                                            conflict!
'V'   C0     linux/ivtv.h                                              conflict!
'V'   C0     media/si4713.h                                            conflict!
'W'   00-1F  linux/watchdog.h                                          conflict!
'W'   00-1F  linux/wanrouter.h                                         conflict! (pre 3.9)
'W'   00-3F  sound/asound.h                                            conflict!
'W'   40-5F  drivers/pci/switch/switchtec.c
'W'   60-61  linux/watch_queue.h
'X'   all    fs/xfs/xfs_fs.h,                                          conflict!
             fs/xfs/linux-2.6/xfs_ioctl32.h,
             include/linux/falloc.h,
             linux/fs.h,
'X'   all    fs/ocfs2/ocfs_fs.h                                        conflict!
'Z'   14-15  drivers/message/fusion/mptctl.h
'['   00-3F  linux/usb/tmc.h                                           USB Test and Measurement Devices
                                                                       <mailto:gregkh@linuxfoundation.org>
'a'   all    linux/atm*.h, linux/sonet.h                               ATM on linux
                                                                       <http://lrcwww.epfl.ch/>
'a'   00-0F  drivers/crypto/qat/qat_common/adf_cfg_common.h            conflict! qat driver
'b'   00-FF                                                            conflict! bit3 vme host bridge
                                                                       <mailto:natalia@nikhefk.nikhef.nl>
'b'   00-0F  linux/dma-buf.h                                           conflict!
'c'   00-7F  linux/comstats.h                                          conflict!
'c'   00-7F  linux/coda.h                                              conflict!
'c'   00-1F  linux/chio.h                                              conflict!
'c'   80-9F  arch/s390/include/asm/chsc.h                              conflict!
'c'   A0-AF  arch/x86/include/asm/msr.h conflict!
'd'   00-FF  linux/char/drm/drm.h                                      conflict!
'd'   02-40  pcmcia/ds.h                                               conflict!
'd'   F0-FF  linux/digi1.h
'e'   all    linux/digi1.h                                             conflict!
'f'   00-1F  linux/ext2_fs.h                                           conflict!
'f'   00-1F  linux/ext3_fs.h                                           conflict!
'f'   00-0F  fs/jfs/jfs_dinode.h                                       conflict!
'f'   00-0F  fs/ext4/ext4.h                                            conflict!
'f'   00-0F  linux/fs.h                                                conflict!
'f'   00-0F  fs/ocfs2/ocfs2_fs.h                                       conflict!
'f'   13-27  linux/fscrypt.h
'f'   81-8F  linux/fsverity.h
'g'   00-0F  linux/usb/gadgetfs.h
'g'   20-2F  linux/usb/g_printer.h
'h'   00-7F                                                            conflict! Charon filesystem
                                                                       <mailto:zapman@interlan.net>
'h'   00-1F  linux/hpet.h                                              conflict!
'h'   80-8F  fs/hfsplus/ioctl.c
'i'   00-3F  linux/i2o-dev.h                                           conflict!
'i'   0B-1F  linux/ipmi.h                                              conflict!
'i'   80-8F  linux/i8k.h
'i'   90-9F  `linux/iio/*.h`                                           IIO
'j'   00-3F  linux/joystick.h
'k'   00-0F  linux/spi/spidev.h                                        conflict!
'k'   00-05  video/kyro.h                                              conflict!
'k'   10-17  linux/hsi/hsi_char.h                                      HSI character device
'l'   00-3F  linux/tcfs_fs.h                                           transparent cryptographic file system
                                                                       <http://web.archive.org/web/%2A/http://mikonos.dia.unisa.it/tcfs>
'l'   40-7F  linux/udf_fs_i.h                                          in development:
                                                                       <https://github.com/pali/udftools>
'm'   00-09  linux/mmtimer.h                                           conflict!
'm'   all    linux/mtio.h                                              conflict!
'm'   all    linux/soundcard.h                                         conflict!
'm'   all    linux/synclink.h                                          conflict!
'm'   00-19  drivers/message/fusion/mptctl.h                           conflict!
'm'   00     drivers/scsi/megaraid/megaraid_ioctl.h                    conflict!
'n'   00-7F  linux/ncp_fs.h and fs/ncpfs/ioctl.c
'n'   80-8F  uapi/linux/nilfs2_api.h                                   NILFS2
'n'   E0-FF  linux/matroxfb.h                                          matroxfb
'o'   00-1F  fs/ocfs2/ocfs2_fs.h                                       OCFS2
'o'   00-03  mtd/ubi-user.h                                            conflict! (OCFS2 and UBI overlaps)
'o'   40-41  mtd/ubi-user.h                                            UBI
'o'   01-A1  `linux/dvb/*.h`                                           DVB
'p'   00-0F  linux/phantom.h                                           conflict! (OpenHaptics needs this)
'p'   00-1F  linux/rtc.h                                               conflict!
'p'   40-7F  linux/nvram.h
'p'   80-9F  linux/ppdev.h                                             user-space parport
                                                                       <mailto:tim@cyberelk.net>
'p'   A1-A5  linux/pps.h                                               LinuxPPS
'p'   B1-B3  linux/pps_gen.h                                           LinuxPPS
                                                                       <mailto:giometti@linux.it>
'q'   00-1F  linux/serio.h
'q'   80-FF  linux/telephony.h                                         Internet PhoneJACK, Internet LineJACK
             linux/ixjuser.h                                           <http://web.archive.org/web/%2A/http://www.quicknet.net>
'r'   00-1F  linux/msdos_fs.h and fs/fat/dir.c
's'   all    linux/cdk.h
't'   00-7F  linux/ppp-ioctl.h
't'   80-8F  linux/isdn_ppp.h
't'   90-91  linux/toshiba.h                                           toshiba and toshiba_acpi SMM
'u'   00-1F  linux/smb_fs.h                                            gone
'u'   00-2F  linux/ublk_cmd.h                                          conflict!
'u'   20-3F  linux/uvcvideo.h                                          USB video class host driver
'u'   40-4f  linux/udmabuf.h                                           userspace dma-buf misc device
'v'   00-1F  linux/ext2_fs.h                                           conflict!
'v'   00-1F  linux/fs.h                                                conflict!
'v'   00-0F  linux/sonypi.h                                            conflict!
'v'   00-0F  media/v4l2-subdev.h                                       conflict!
'v'   20-27  arch/powerpc/include/uapi/asm/vas-api.h                   VAS API
'v'   C0-FF  linux/meye.h                                              conflict!
'w'   all                                                              CERN SCI driver
'y'   00-1F                                                            packet based user level communications
                                                                       <mailto:zapman@interlan.net>
'z'   00-3F                                                            CAN bus card conflict!
                                                                       <mailto:hdstich@connectu.ulm.circular.de>
'z'   40-7F                                                            CAN bus card conflict!
                                                                       <mailto:oe@port.de>
'z'   10-4F  drivers/s390/crypto/zcrypt_api.h                          conflict!
'|'   00-7F  linux/media.h
'|'   80-9F  samples/                                                  Any sample and example drivers
0x80  00-1F  linux/fb.h
0x81  00-1F  linux/vduse.h
0x89  00-06  arch/x86/include/asm/sockios.h
0x89  0B-DF  linux/sockios.h
0x89  E0-EF  linux/sockios.h                                           SIOCPROTOPRIVATE range
0x89  F0-FF  linux/sockios.h                                           SIOCDEVPRIVATE range
0x8A  00-1F  linux/eventpoll.h
0x8B  all    linux/wireless.h
0x8C  00-3F                                                            WiNRADiO driver
                                                                       <http://www.winradio.com.au/>
0x90  00     drivers/cdrom/sbpcd.h
0x92  00-0F  drivers/usb/mon/mon_bin.c
0x93  60-7F  linux/auto_fs.h
0x94  all    fs/btrfs/ioctl.h                                          Btrfs filesystem
             and linux/fs.h                                            some lifted to vfs/generic
0x97  00-7F  fs/ceph/ioctl.h                                           Ceph file system
0x99  00-0F                                                            537-Addinboard driver
                                                                       <mailto:buk@buks.ipn.de>
0x9A  00-0F  include/uapi/fwctl/fwctl.h
0xA0  all    linux/sdp/sdp.h                                           Industrial Device Project
                                                                       <mailto:kenji@bitgate.com>
0xA1  0      linux/vtpm_proxy.h                                        TPM Emulator Proxy Driver
0xA2  all    uapi/linux/acrn.h                                         ACRN hypervisor
0xA3  80-8F                                                            Port ACL  in development:
                                                                       <mailto:tlewis@mindspring.com>
0xA3  90-9F  linux/dtlk.h
0xA4  00-1F  uapi/linux/tee.h                                          Generic TEE subsystem
0xA4  00-1F  uapi/asm/sgx.h                                            <mailto:linux-sgx@vger.kernel.org>
0xA5  01-05  linux/surface_aggregator/cdev.h                           Microsoft Surface Platform System Aggregator
                                                                       <mailto:luzmaximilian@gmail.com>
0xA5  20-2F  linux/surface_aggregator/dtx.h                            Microsoft Surface DTX driver
                                                                       <mailto:luzmaximilian@gmail.com>
0xAA  00-3F  linux/uapi/linux/userfaultfd.h
0xAB  00-1F  linux/nbd.h
0xAC  00-1F  linux/raw.h
0xAD  00                                                               Netfilter device in development:
                                                                       <mailto:rusty@rustcorp.com.au>
0xAE  00-1F  linux/kvm.h                                               Kernel-based Virtual Machine
                                                                       <mailto:kvm@vger.kernel.org>
0xAE  40-FF  linux/kvm.h                                               Kernel-based Virtual Machine
                                                                       <mailto:kvm@vger.kernel.org>
0xAE  20-3F  linux/nitro_enclaves.h                                    Nitro Enclaves
0xAF  00-1F  linux/fsl_hypervisor.h                                    Freescale hypervisor
0xB0  all                                                              RATIO devices in development:
                                                                       <mailto:vgo@ratio.de>
0xB1  00-1F                                                            PPPoX
                                                                       <mailto:mostrows@styx.uwaterloo.ca>
0xB2  00     arch/powerpc/include/uapi/asm/papr-vpd.h                  powerpc/pseries VPD API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB2  01-02  arch/powerpc/include/uapi/asm/papr-sysparm.h              powerpc/pseries system parameter API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB2  03-05  arch/powerpc/include/uapi/asm/papr-indices.h              powerpc/pseries indices API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB2  06-07  arch/powerpc/include/uapi/asm/papr-platform-dump.h        powerpc/pseries Platform Dump API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB2  08     arch/powerpc/include/uapi/asm/papr-physical-attestation.h powerpc/pseries Physical Attestation API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB2  09     arch/powerpc/include/uapi/asm/papr-hvpipe.h               powerpc/pseries HVPIPE API
                                                                       <mailto:linuxppc-dev@lists.ozlabs.org>
0xB3  00     linux/mmc/ioctl.h
0xB4  00-0F  linux/gpio.h                                              <mailto:linux-gpio@vger.kernel.org>
0xB5  00-0F  uapi/linux/rpmsg.h                                        <mailto:linux-remoteproc@vger.kernel.org>
0xB6  all    linux/fpga-dfl.h
0xB7  all    uapi/linux/remoteproc_cdev.h                              <mailto:linux-remoteproc@vger.kernel.org>
0xB7  all    uapi/linux/nsfs.h                                         <mailto:Andrei Vagin <avagin@openvz.org>>
0xB8  01-02  uapi/misc/mrvl_cn10k_dpi.h                                Marvell CN10K DPI driver
0xB8  all    uapi/linux/mshv.h                                         Microsoft Hyper-V /dev/mshv driver
                                                                       <mailto:linux-hyperv@vger.kernel.org>
0xBA  00-0F  uapi/linux/liveupdate.h                                   Pasha Tatashin
                                                                       <mailto:pasha.tatashin@soleen.com>
0xC0  00-0F  linux/usb/iowarrior.h
0xCA  00-0F  uapi/misc/cxl.h                                           Dead since 6.15
0xCA  10-2F  uapi/misc/ocxl.h
0xCA  80-BF  uapi/scsi/cxlflash_ioctl.h                                Dead since 6.15
0xCB  00-1F                                                            CBM serial IEC bus in development:
                                                                       <mailto:michael.klein@puffin.lb.shuttle.de>
0xCC  00-0F  drivers/misc/ibmvmc.h                                     pseries VMC driver
0xCD  01     linux/reiserfs_fs.h                                       Dead since 6.13
0xCE  01-02  uapi/linux/cxl_mem.h                                      Compute Express Link Memory Devices
0xCF  02     fs/smb/client/cifs_ioctl.h
0xDB  00-0F  drivers/char/mwave/mwavepub.h
0xDD  00-3F                                                            ZFCP device driver see drivers/s390/scsi/
                                                                       <mailto:aherrman@de.ibm.com>
0xE5  00-3F  linux/fuse.h
0xEC  00-01  drivers/platform/chrome/cros_ec_dev.h                     ChromeOS EC driver
0xEE  00-09  uapi/linux/pfrut.h                                        Platform Firmware Runtime Update and Telemetry
0xF3  00-3F  drivers/usb/misc/sisusbvga/sisusb.h                       sisfb (in development)
                                                                       <mailto:thomas@winischhofer.net>
0xF6  all                                                              LTTng Linux Trace Toolkit Next Generation
                                                                       <mailto:mathieu.desnoyers@efficios.com>
0xF8  all    arch/x86/include/uapi/asm/amd_hsmp.h                      AMD HSMP EPYC system management interface driver
                                                                       <mailto:nchatrad@amd.com>
0xF9  00-0F  uapi/misc/amd-apml.h                                      AMD side band system management interface driver
                                                                       <mailto:naveenkrishna.chatradhi@amd.com>
0xFD  all    linux/dm-ioctl.h
0xFE  all    linux/isst_if.h
====  =====  ========================================================= ================================================================
--
/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * Copyright (c) 1999-2002 Vojtech Pavlik
 *
 * This program is free software; you can redistribute it and/or modify it
 * under the terms of the GNU General Public License version 2 as published by
 * the Free Software Foundation.
 */
#ifndef _UAPI_INPUT_H
#define _UAPI_INPUT_H


#ifndef __KERNEL__
#include <sys/time.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <linux/types.h>
#endif

#include "input-event-codes.h"

/*
 * The event structure itself
 * Note that __USE_TIME_BITS64 is defined by libc based on
 * application's request to use 64 bit time_t.
 */

struct input_event {
#if (__BITS_PER_LONG != 32 || !defined(__USE_TIME_BITS64)) && !defined(__KERNEL__)
	struct timeval time;
#define input_event_sec time.tv_sec
#define input_event_usec time.tv_usec
#else
	__kernel_ulong_t __sec;
#if defined(__sparc__) && defined(__arch64__)
	unsigned int __usec;
	unsigned int __pad;
#else
	__kernel_ulong_t __usec;
#endif
#define input_event_sec  __sec
#define input_event_usec __usec
#endif
	__u16 type;
	__u16 code;
	__s32 value;
};

/*
 * Protocol version.
 */

#define EV_VERSION		0x010001

/*
 * IOCTLs (0x00 - 0x7f)
 */

struct input_id {
	__u16 bustype;
	__u16 vendor;
	__u16 product;
	__u16 version;
};

/**
 * struct input_absinfo - used by EVIOCGABS/EVIOCSABS ioctls
 * @value: latest reported value for the axis.
 * @minimum: specifies minimum value for the axis.
 * @maximum: specifies maximum value for the axis.
 * @fuzz: specifies fuzz value that is used to filter noise from
 *	the event stream.
 * @flat: values that are within this value will be discarded by
 *	joydev interface and reported as 0 instead.
 * @resolution: specifies resolution for the values reported for
 *	the axis.
 *
 * Note that input core does not clamp reported values to the
 * [minimum, maximum] limits, such task is left to userspace.
 *
 * The default resolution for main axes (ABS_X, ABS_Y, ABS_Z,
 * ABS_MT_POSITION_X, ABS_MT_POSITION_Y) is reported in units
 * per millimeter (units/mm), resolution for rotational axes
 * (ABS_RX, ABS_RY, ABS_RZ) is reported in units per radian.
 * The resolution for the size axes (ABS_MT_TOUCH_MAJOR,
 * ABS_MT_TOUCH_MINOR, ABS_MT_WIDTH_MAJOR, ABS_MT_WIDTH_MINOR)
 * is reported in units per millimeter (units/mm).
 * When INPUT_PROP_ACCELEROMETER is set the resolution changes.
 * The main axes (ABS_X, ABS_Y, ABS_Z) are then reported in
 * units per g (units/g) and in units per degree per second
 * (units/deg/s) for rotational axes (ABS_RX, ABS_RY, ABS_RZ).
 */
struct input_absinfo {
	__s32 value;
	__s32 minimum;
	__s32 maximum;
	__s32 fuzz;
	__s32 flat;
	__s32 resolution;
};

/**
 * struct input_keymap_entry - used by EVIOCGKEYCODE/EVIOCSKEYCODE ioctls
 * @scancode: scancode represented in machine-endian form.
 * @len: length of the scancode that resides in @scancode buffer.
 * @index: index in the keymap, may be used instead of scancode
 * @flags: allows to specify how kernel should handle the request. For
 *	example, setting INPUT_KEYMAP_BY_INDEX flag indicates that kernel
 *	should perform lookup in keymap by @index instead of @scancode
 * @keycode: key code assigned to this scancode
 *
 * The structure is used to retrieve and modify keymap data. Users have
 * option of performing lookup either by @scancode itself or by @index
 * in keymap entry. EVIOCGKEYCODE will also return scancode or index
 * (depending on which element was used to perform lookup).
 */
struct input_keymap_entry {
#define INPUT_KEYMAP_BY_INDEX	(1 << 0)
	__u8  flags;
	__u8  len;
	__u16 index;
	__u32 keycode;
	__u8  scancode[32];
};

struct input_mask {
	__u32 type;
	__u32 codes_size;
	__u64 codes_ptr;
};

#define EVIOCGVERSION		_IOR('E', 0x01, int)			/* get driver version */
#define EVIOCGID		_IOR('E', 0x02, struct input_id)	/* get device ID */
#define EVIOCGREP		_IOR('E', 0x03, unsigned int[2])	/* get repeat settings */
#define EVIOCSREP		_IOW('E', 0x03, unsigned int[2])	/* set repeat settings */

#define EVIOCGKEYCODE		_IOR('E', 0x04, unsigned int[2])        /* get keycode */
#define EVIOCGKEYCODE_V2	_IOR('E', 0x04, struct input_keymap_entry)
#define EVIOCSKEYCODE		_IOW('E', 0x04, unsigned int[2])        /* set keycode */
#define EVIOCSKEYCODE_V2	_IOW('E', 0x04, struct input_keymap_entry)

#define EVIOCGNAME(len)		_IOC(_IOC_READ, 'E', 0x06, len)		/* get device name */
#define EVIOCGPHYS(len)		_IOC(_IOC_READ, 'E', 0x07, len)		/* get physical location */
#define EVIOCGUNIQ(len)		_IOC(_IOC_READ, 'E', 0x08, len)		/* get unique identifier */
#define EVIOCGPROP(len)		_IOC(_IOC_READ, 'E', 0x09, len)		/* get device properties */

/**
 * EVIOCGMTSLOTS(len) - get MT slot values
 * @len: size of the data buffer in bytes
 *
 * The ioctl buffer argument should be binary equivalent to
 *
 * struct input_mt_request_layout {
 *	__u32 code;
 *	__s32 values[num_slots];
 * };
 *
 * where num_slots is the (arbitrary) number of MT slots to extract.
 *
 * The ioctl size argument (len) is the size of the buffer, which
 * should satisfy len = (num_slots + 1) * sizeof(__s32).  If len is
 * too small to fit all available slots, the first num_slots are
 * returned.
 *
 * Before the call, code is set to the wanted ABS_MT event type. On
 * return, values[] is filled with the slot values for the specified
 * ABS_MT code.
 *
 * If the request code is not an ABS_MT value, -EINVAL is returned.
 */
#define EVIOCGMTSLOTS(len)	_IOC(_IOC_READ, 'E', 0x0a, len)

#define EVIOCGKEY(len)		_IOC(_IOC_READ, 'E', 0x18, len)		/* get global key state */
#define EVIOCGLED(len)		_IOC(_IOC_READ, 'E', 0x19, len)		/* get all LEDs */
#define EVIOCGSND(len)		_IOC(_IOC_READ, 'E', 0x1a, len)		/* get all sounds status */
#define EVIOCGSW(len)		_IOC(_IOC_READ, 'E', 0x1b, len)		/* get all switch states */

#define EVIOCGBIT(ev,len)	_IOC(_IOC_READ, 'E', 0x20 + (ev), len)	/* get event bits */
#define EVIOCGABS(abs)		_IOR('E', 0x40 + (abs), struct input_absinfo)	/* get abs value/limits */
#define EVIOCSABS(abs)		_IOW('E', 0xc0 + (abs), struct input_absinfo)	/* set abs value/limits */

#define EVIOCSFF		_IOW('E', 0x80, struct ff_effect)	/* send a force effect to a force feedback device */
#define EVIOCRMFF		_IOW('E', 0x81, int)			/* Erase a force effect */
#define EVIOCGEFFECTS		_IOR('E', 0x84, int)			/* Report number of effects playable at the same time */

#define EVIOCGRAB		_IOW('E', 0x90, int)			/* Grab/Release device */
#define EVIOCREVOKE		_IOW('E', 0x91, int)			/* Revoke device access */

/**
 * EVIOCGMASK - Retrieve current event mask
 *
 * This ioctl allows user to retrieve the current event mask for specific
 * event type. The argument must be of type "struct input_mask" and
 * specifies the event type to query, the address of the receive buffer and
 * the size of the receive buffer.
 *
 * The event mask is a per-client mask that specifies which events are
 * forwarded to the client. Each event code is represented by a single bit
 * in the event mask. If the bit is set, the event is passed to the client
 * normally. Otherwise, the event is filtered and will never be queued on
 * the client's receive buffer.
 *
 * Event masks do not affect global state of the input device. They only
 * affect the file descriptor they are applied to.
 *
 * The default event mask for a client has all bits set, i.e. all events
 * are forwarded to the client. If the kernel is queried for an unknown
 * event type or if the receive buffer is larger than the number of
 * event codes known to the kernel, the kernel returns all zeroes for those
 * codes.
 *
 * At maximum, codes_size bytes are copied.
 *
 * This ioctl may fail with ENODEV in case the file is revoked, EFAULT
 * if the receive-buffer points to invalid memory, or EINVAL if the kernel
 * does not implement the ioctl.
 */
#define EVIOCGMASK		_IOR('E', 0x92, struct input_mask)	/* Get event-masks */

/**
 * EVIOCSMASK - Set event mask
 *
 * This ioctl is the counterpart to EVIOCGMASK. Instead of receiving the
 * current event mask, this changes the client's event mask for a specific
 * type.  See EVIOCGMASK for a description of event-masks and the
 * argument-type.
 *
 * This ioctl provides full forward compatibility. If the passed event type
 * is unknown to the kernel, or if the number of event codes specified in
 * the mask is bigger than what is known to the kernel, the ioctl is still
 * accepted and applied. However, any unknown codes are left untouched and
 * stay cleared. That means, the kernel always filters unknown codes
 * regardless of what the client requests.  If the new mask doesn't cover
 * all known event-codes, all remaining codes are automatically cleared and
 * thus filtered.
 *
 * This ioctl may fail with ENODEV in case the file is revoked. EFAULT is
 * returned if the receive-buffer points to invalid memory. EINVAL is returned
 * if the kernel does not implement the ioctl.
 */
#define EVIOCSMASK		_IOW('E', 0x93, struct input_mask)	/* Set event-masks */

#define EVIOCSCLOCKID		_IOW('E', 0xa0, int)			/* Set clockid to be used for timestamps */

/*
 * IDs.
 */

#define ID_BUS			0
#define ID_VENDOR		1
#define ID_PRODUCT		2
#define ID_VERSION		3

#define BUS_PCI			0x01
#define BUS_ISAPNP		0x02
#define BUS_USB			0x03
#define BUS_HIL			0x04
#define BUS_BLUETOOTH		0x05
#define BUS_VIRTUAL		0x06

#define BUS_ISA			0x10
#define BUS_I8042		0x11
#define BUS_XTKBD		0x12
#define BUS_RS232		0x13
#define BUS_GAMEPORT		0x14
#define BUS_PARPORT		0x15
#define BUS_AMIGA		0x16
#define BUS_ADB			0x17
#define BUS_I2C			0x18
#define BUS_HOST		0x19
#define BUS_GSC			0x1A
#define BUS_ATARI		0x1B
#define BUS_SPI			0x1C
#define BUS_RMI			0x1D
#define BUS_CEC			0x1E
#define BUS_INTEL_ISHTP		0x1F
#define BUS_AMD_SFH		0x20
#define BUS_SDW			0x21

/*
 * MT_TOOL types
 */
#define MT_TOOL_FINGER		0x00
#define MT_TOOL_PEN		0x01
#define MT_TOOL_PALM		0x02
#define MT_TOOL_DIAL		0x0a
#define MT_TOOL_MAX		0x0f

/*
 * Values describing the status of a force-feedback effect
 */
#define FF_STATUS_STOPPED	0x00
#define FF_STATUS_PLAYING	0x01
#define FF_STATUS_MAX		0x01

/*
 * Structures used in ioctls to upload effects to a device
 * They are pieces of a bigger structure (called ff_effect)
 */

/*
 * All duration values are expressed in ms. Values above 32767 ms (0x7fff)
 * should not be used and have unspecified results.
 */

/**
 * struct ff_replay - defines scheduling of the force-feedback effect
 * @length: duration of the effect
 * @delay: delay before effect should start playing
 */
struct ff_replay {
	__u16 length;
	__u16 delay;
};

/**
 * struct ff_trigger - defines what triggers the force-feedback effect
 * @button: number of the button triggering the effect
 * @interval: controls how soon the effect can be re-triggered
 */
struct ff_trigger {
	__u16 button;
	__u16 interval;
};

/**
 * struct ff_envelope - generic force-feedback effect envelope
 * @attack_length: duration of the attack (ms)
 * @attack_level: level at the beginning of the attack
 * @fade_length: duration of fade (ms)
 * @fade_level: level at the end of fade
 *
 * The @attack_level and @fade_level are absolute values; when applying
 * envelope force-feedback core will convert to positive/negative
 * value based on polarity of the default level of the effect.
 * Valid range for the attack and fade levels is 0x0000 - 0x7fff
 */
struct ff_envelope {
	__u16 attack_length;
	__u16 attack_level;
	__u16 fade_length;
	__u16 fade_level;
};

/**
 * struct ff_constant_effect - defines parameters of a constant force-feedback effect
 * @level: strength of the effect; may be negative
 * @envelope: envelope data
 */
struct ff_constant_effect {
	__s16 level;
	struct ff_envelope envelope;
};

/**
 * struct ff_ramp_effect - defines parameters of a ramp force-feedback effect
 * @start_level: beginning strength of the effect; may be negative
 * @end_level: final strength of the effect; may be negative
 * @envelope: envelope data
 */
struct ff_ramp_effect {
	__s16 start_level;
	__s16 end_level;
	struct ff_envelope envelope;
};

/**
 * struct ff_condition_effect - defines a spring or friction force-feedback effect
 * @right_saturation: maximum level when joystick moved all way to the right
 * @left_saturation: same for the left side
 * @right_coeff: controls how fast the force grows when the joystick moves
 *	to the right
 * @left_coeff: same for the left side
 * @deadband: size of the dead zone, where no force is produced
 * @center: position of the dead zone
 */
struct ff_condition_effect {
	__u16 right_saturation;
	__u16 left_saturation;

	__s16 right_coeff;
	__s16 left_coeff;

	__u16 deadband;
	__s16 center;
};

/**
 * struct ff_periodic_effect - defines parameters of a periodic force-feedback effect
 * @waveform: kind of the effect (wave)
 * @period: period of the wave (ms)
 * @magnitude: peak value
 * @offset: mean value of the wave (roughly)
 * @phase: 'horizontal' shift
 * @envelope: envelope data
 * @custom_len: number of samples (FF_CUSTOM only)
 * @custom_data: buffer of samples (FF_CUSTOM only)
 *
 * Known waveforms - FF_SQUARE, FF_TRIANGLE, FF_SINE, FF_SAW_UP,
 * FF_SAW_DOWN, FF_CUSTOM. The exact syntax FF_CUSTOM is undefined
 * for the time being as no driver supports it yet.
 *
 * Note: the data pointed by custom_data is copied by the driver.
 * You can therefore dispose of the memory after the upload/update.
 */
struct ff_periodic_effect {
	__u16 waveform;
	__u16 period;
	__s16 magnitude;
	__s16 offset;
	__u16 phase;

	struct ff_envelope envelope;

	__u32 custom_len;
	__s16 __user *custom_data;
};

/**
 * struct ff_rumble_effect - defines parameters of a periodic force-feedback effect
 * @strong_magnitude: magnitude of the heavy motor
 * @weak_magnitude: magnitude of the light one
 *
 * Some rumble pads have two motors of different weight. Strong_magnitude
 * represents the magnitude of the vibration generated by the heavy one.
 */
struct ff_rumble_effect {
	__u16 strong_magnitude;
	__u16 weak_magnitude;
};

/**
 * struct ff_haptic_effect
 * @hid_usage: hid_usage according to Haptics page (WAVEFORM_CLICK, etc.)
 * @vendor_id: the waveform vendor ID if hid_usage is in the vendor-defined range
 * @vendor_waveform_page: the vendor waveform page if hid_usage is in the vendor-defined range
 * @intensity: strength of the effect as percentage
 * @repeat_count: number of times to retrigger effect
 * @retrigger_period: time before effect is retriggered (in ms)
 */
struct ff_haptic_effect {
	__u16 hid_usage;
	__u16 vendor_id;
	__u8  vendor_waveform_page;
	__u16 intensity;
	__u16 repeat_count;
	__u16 retrigger_period;
};

/**
 * struct ff_effect - defines force feedback effect
 * @type: type of the effect (FF_CONSTANT, FF_PERIODIC, FF_RAMP, FF_SPRING,
 *	FF_FRICTION, FF_DAMPER, FF_RUMBLE, FF_INERTIA, or FF_CUSTOM)
 * @id: an unique id assigned to an effect
 * @direction: direction of the effect
 * @trigger: trigger conditions (struct ff_trigger)
 * @replay: scheduling of the effect (struct ff_replay)
 * @u: effect-specific structure (one of ff_constant_effect, ff_ramp_effect,
 *	ff_periodic_effect, ff_condition_effect, ff_rumble_effect) further
 *	defining effect parameters
 *
 * This structure is sent through ioctl from the application to the driver.
 * To create a new effect application should set its @id to -1; the kernel
 * will return assigned @id which can later be used to update or delete
 * this effect.
 *
 * Direction of the effect is encoded as follows:
 *	0 deg -> 0x0000 (down)
 *	90 deg -> 0x4000 (left)
 *	180 deg -> 0x8000 (up)
 *	270 deg -> 0xC000 (right)
 */
struct ff_effect {
	__u16 type;
	__s16 id;
	__u16 direction;
	struct ff_trigger trigger;
	struct ff_replay replay;

	union {
		struct ff_constant_effect constant;
		struct ff_ramp_effect ramp;
		struct ff_periodic_effect periodic;
		struct ff_condition_effect condition[2]; /* One for each axis */
		struct ff_rumble_effect rumble;
		struct ff_haptic_effect haptic;
	} u;
};

/*
 * Force feedback effect types
 */

#define FF_HAPTIC		0x4f
#define FF_RUMBLE	0x50
#define FF_PERIODIC	0x51
#define FF_CONSTANT	0x52
#define FF_SPRING	0x53
#define FF_FRICTION	0x54
#define FF_DAMPER	0x55
#define FF_INERTIA	0x56
#define FF_RAMP		0x57

#define FF_EFFECT_MIN	FF_HAPTIC
#define FF_EFFECT_MAX	FF_RAMP

/*
 * Force feedback periodic effect types
 */

#define FF_SQUARE	0x58
#define FF_TRIANGLE	0x59
#define FF_SINE		0x5a
#define FF_SAW_UP	0x5b
#define FF_SAW_DOWN	0x5c
#define FF_CUSTOM	0x5d

#define FF_WAVEFORM_MIN	FF_SQUARE
#define FF_WAVEFORM_MAX	FF_CUSTOM

/*
 * Set ff device properties
 */

#define FF_GAIN		0x60
#define FF_AUTOCENTER	0x61

/*
 * ff->playback(effect_id = FF_GAIN) is the first effect_id to
 * cause a collision with another ff method, in this case ff->set_gain().
 * Therefore the greatest safe value for effect_id is FF_GAIN - 1,
 * and thus the total number of effects should never exceed FF_GAIN.
 */
#define FF_MAX_EFFECTS	FF_GAIN

#define FF_MAX		0x7f
#define FF_CNT		(FF_MAX+1)

#endif /* _UAPI_INPUT_H */

