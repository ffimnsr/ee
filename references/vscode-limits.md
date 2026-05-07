According to Google Gemini

Key Default Limits

Large File Optimization (20 MB or 300,000 lines): Any file exceeding 30 MB or 300,000 lines is automatically treated as a "large file" [9, 16]. To save memory, VS Code disables features like tokenization (syntax highlighting), word wrapping, and certain extension interactions for these files [9].

Open Confirmation Warning (1,024 MB / 1 GB): By default, VS Code will show a confirmation prompt before attempting to open any file larger than 1,024 MB to prevent accidental freezes or excessive memory consumption [3, 17].

Saving Files: VS Code can save files larger than 256 MB by streaming them to disk in chunks rather than loading the entire content into memory [10].

Diff/Comparison: The built-in file comparison tool has a default limit of 50 MB [37].

Local History: Files larger than 256 KB are excluded from the local timeline history by default to save disk space [8].

JSON Processing: For JSON files, VS Code limits the calculation of folding regions and outline symbols to 5,000 items to maintain performance [11].


Emacs VLF can open terabytes of sizes in file with VLF
https://github.com/m00natic/vlfi
