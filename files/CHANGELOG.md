# Changelog

## Unreleased

- No longer reports the selected file types as signals. A file manager gives no event when the selection changes, and polling for it costs too much; a profile can't switch on the selection until one does. The actions still work on whatever is selected when you run them.

## 1.2.0

- Profile rules can test what is selected: the file types (`png`, `pdf`…) and kinds (image, video, audio, PDF, archive, text, folder) of the selected files are signals. The dial reads them as it opens; a phone's deck follows them while it is connected. They have no value outside a file manager, or for a selection with a type this plugin doesn't list.

## 1.1.1

- A proper app icon in the Plugins store and sidebar (`icons/app-icon.svg`).

