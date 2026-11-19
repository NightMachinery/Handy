# Bluetooth headphone volume during dictation on macOS

Opening a Bluetooth headset's microphone can switch it from stereo playback to two-way audio. macOS can select a higher output volume during that switch, making other apps' playback louder. [Apple's explanation of Bluetooth audio modes](https://support.apple.com/en-ie/102217)

Handy reads the headset's output volume before opening its microphone. During the switch it checks the Core Audio output volume and lowers an increase to the previous setting. This works with **Mute While Recording** on or off. A quieter recording profile keeps its level. The check finishes after the switch so the volume keys work normally during dictation.

The guard runs when a capture stream opens, including always-on startup and reconnection. It checks that the default output belongs to the selected Bluetooth microphone and writes directly to that output's virtual main volume control, which preserves channel balance. The guard leaves the mute control to Handy's existing feedback and mute sequence.

To keep stereo playback during dictation, choose the Mac's built-in mic (with the lid open) or a USB mic in **General → Sound → Microphone** and keep the headphones selected for output.

If only Handy's start and stop tones are loud, turn off **Audio Feedback** for one recording. If your own voice sounds amplified, some Bose models have a **Self Voice** setting in the Bose app that controls how much of your voice you hear during calls. [Bose QuietComfort Ultra user guide](https://assets.bose.com/content/dam/Bose_DAM/Web/consumer_electronics/global/products/headphones/qcuh2-headphonearn/pdfs/897194_OG_QCUH2-HEADPHONEARN_ML.pdf)

With on-demand recording, **Mute While Recording** leaves the start and stop tones audible. Handy opens the microphone, plays the start tone if enabled, then mutes system output. On stop it restores the previous mute state before playing the stop tone and closing the microphone. These intervals can expose a playback level change at either end of a muted recording.
